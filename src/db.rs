use anyhow::Result;
use sqlx::{postgres::PgPoolOptions, Pool, Postgres, Row};
use tracing::info;
use std::collections::HashMap;

use crate::models::{FirehoseCursor, NotificationPreference, UserDevice};

pub async fn init_db_pool(database_url: &str) -> Result<Pool<Postgres>> {
    info!("Initializing database connection pool");
    
    // Calculate optimal connection count based on CPU cores
    let max_connections = std::env::var("DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or_else(|| {
            let cores = num_cpus::get() as u32;
            cores * 2 + 1 // Common formula for connection pools
        });
    
    info!("Setting database pool to {} max connections", max_connections);
    
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
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

pub async fn get_user_devices_batch(
    pool: &Pool<Postgres>, 
    dids: &[String]
) -> Result<HashMap<String, Vec<UserDevice>>> {
    if dids.is_empty() {
        return Ok(HashMap::new());
    }
    
    let mut result = HashMap::new();
    
    // Process in chunks to avoid too many parameters
    for chunk in dids.chunks(10) {
        // Create placeholders for SQL IN clause
        let placeholders: Vec<String> = (1..=chunk.len())
            .map(|i| format!("${}", i))
            .collect();
        
        let query = format!(
            "SELECT id, did, device_token, created_at, updated_at 
             FROM user_devices 
             WHERE did IN ({})",
            placeholders.join(",")
        );
        
        // Manually build and execute the query
        let mut q = sqlx::query(&query);
        for did in chunk {
            q = q.bind(did);
        }
        
        // Execute the query and process rows
        let rows = q.fetch_all(pool).await?;
        
        for row in rows {
            let device = UserDevice {
                id: row.get("id"),
                did: row.get("did"),
                device_token: row.get("device_token"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            };
            
            result.entry(device.did.clone())
                .or_insert_with(Vec::new)
                .push(device);
        }
    }
    
    Ok(result)
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
