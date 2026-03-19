use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, Pool, Postgres, Row};
use tracing::info;

use crate::models::FirehoseCursor;

pub async fn init_db_pool(database_url: &str) -> Result<Pool<Postgres>> {
    info!("Initializing database connection pool");

    let max_connections = std::env::var("DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or_else(|| {
            let cores = num_cpus::get() as u32;
            cores * 2 + 1
        });

    info!(
        "Setting database pool to {} max connections",
        max_connections
    );

    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await?;

    Ok(pool)
}

/// Create firehose_cursor table if it doesn't exist.
/// All other tables (user_devices, push_event_queue, activity_subscriptions)
/// are managed by nest's migrations.
pub async fn ensure_firehose_cursor_table(pool: &Pool<Postgres>) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS firehose_cursor (
            id SERIAL PRIMARY KEY,
            cursor TEXT NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_registered_users(pool: &Pool<Postgres>) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT DISTINCT did FROM user_devices WHERE is_active = TRUE")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.get("did")).collect())
}

pub async fn get_last_cursor(pool: &Pool<Postgres>) -> Result<Option<String>> {
    let cursor = sqlx::query_as::<_, FirehoseCursor>(
        r#"
        SELECT id, cursor, updated_at
        FROM firehose_cursor
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await?;

    Ok(cursor.map(|c| c.cursor))
}

pub async fn update_cursor(pool: &Pool<Postgres>, cursor: &str) -> Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM firehose_cursor")
        .fetch_one(pool)
        .await?;

    if count > 0 {
        sqlx::query(
            r#"
            UPDATE firehose_cursor
            SET cursor = $1, updated_at = NOW()
            WHERE id = (SELECT id FROM firehose_cursor ORDER BY id DESC LIMIT 1)
            "#,
        )
        .bind(cursor)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            r#"
            INSERT INTO firehose_cursor (cursor, updated_at)
            VALUES ($1, NOW())
            "#,
        )
        .bind(cursor)
        .execute(pool)
        .await?;
    }

    Ok(())
}

pub async fn cleanup_old_cursors(pool: &Pool<Postgres>, days_to_keep: i32) -> Result<()> {
    sqlx::query(
        r#"
        DELETE FROM firehose_cursor
        WHERE updated_at < NOW() - INTERVAL '1 day' * $1
        AND id NOT IN (SELECT id FROM firehose_cursor ORDER BY updated_at DESC LIMIT 1)
        "#,
    )
    .bind(days_to_keep as f64)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get activity subscribers for a given subject DID.
/// Returns (subscriber_did, include_posts, include_replies).
pub async fn get_activity_subscribers(
    pool: &Pool<Postgres>,
    subject_did: &str,
) -> Result<Vec<(String, bool, bool)>> {
    let rows = sqlx::query(
        r#"
        SELECT subscriber_did, include_posts, include_replies
        FROM activity_subscriptions
        WHERE subject_did = $1
        "#,
    )
    .bind(subject_did)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("subscriber_did"),
                r.get::<bool, _>("include_posts"),
                r.get::<bool, _>("include_replies"),
            )
        })
        .collect())
}

/// Enqueue an event into nest's push_event_queue for delivery.
pub async fn enqueue_push_event(
    pool: &Pool<Postgres>,
    recipient_did: &str,
    actor_did: &str,
    notification_type: &str,
    event_cid: &str,
    event_path: &str,
    subject_uri: Option<&str>,
    thread_root_uri: Option<&str>,
    event_record_json: &serde_json::Value,
    event_timestamp: i64,
    dedupe_key: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO push_event_queue (
            recipient_did, actor_did, notification_type,
            event_cid, event_path, subject_uri, thread_root_uri,
            event_record_json, event_timestamp, dedupe_key
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (dedupe_key) DO NOTHING
        "#,
    )
    .bind(recipient_did)
    .bind(actor_did)
    .bind(notification_type)
    .bind(event_cid)
    .bind(event_path)
    .bind(subject_uri)
    .bind(thread_root_uri)
    .bind(event_record_json)
    .bind(event_timestamp)
    .bind(dedupe_key)
    .execute(pool)
    .await?;

    Ok(())
}
