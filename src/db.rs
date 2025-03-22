use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, Pool, Postgres};
use tracing::info;

use crate::models::{FirehoseCursor, NotificationPreference, UserDevice};

pub async fn init_db_pool(database_url: &str) -> Result<Pool<Postgres>> {
    info!("Initializing database connection pool");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;

    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

pub async fn get_user_devices(pool: &Pool<Postgres>, did: &str) -> Result<Vec<UserDevice>> {
    let devices = sqlx::query_as!(
        UserDevice,
        r#"
        SELECT id, did, device_token, created_at, updated_at
        FROM user_devices
        WHERE did = $1
        "#,
        did
    )
    .fetch_all(pool)
    .await?;

    Ok(devices)
}

pub async fn get_notification_preferences(
    pool: &Pool<Postgres>,
    user_id: uuid::Uuid,
) -> Result<NotificationPreference> {
    let preferences = sqlx::query_as!(
        NotificationPreference,
        r#"
        SELECT user_id, mentions, replies, likes, follows, reposts, quotes
        FROM notification_preferences
        WHERE user_id = $1
        "#,
        user_id
    )
    .fetch_one(pool)
    .await?;

    Ok(preferences)
}

pub async fn get_last_cursor(pool: &Pool<Postgres>) -> Result<Option<String>> {
    let cursor = sqlx::query_as!(
        FirehoseCursor,
        r#"
        SELECT id, cursor, updated_at
        FROM firehose_cursor
        ORDER BY id DESC
        LIMIT 1
        "#
    )
    .fetch_optional(pool)
    .await?;

    Ok(cursor.map(|c| c.cursor))
}

pub async fn update_cursor(pool: &Pool<Postgres>, cursor: &str) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO firehose_cursor (cursor, updated_at)
        VALUES ($1, NOW())
        "#,
        cursor
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn get_registered_users(pool: &Pool<Postgres>) -> Result<Vec<String>> {
    let users = sqlx::query!(
        r#"
        SELECT DISTINCT did FROM user_devices
        "#
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| row.did)
    .collect();

    Ok(users)
}
