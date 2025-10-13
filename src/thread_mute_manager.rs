use anyhow::{Context, Result};
use moka::future::Cache;
use sqlx::{Pool, Postgres};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{error, info};

pub struct ThreadMuteManager {
    // Cache: user_did -> set of muted thread root URIs
    thread_mutes_cache: Cache<String, HashSet<String>>,
    db_pool: Pool<Postgres>,
}

impl ThreadMuteManager {
    pub fn new(db_pool: Pool<Postgres>) -> Self {
        let thread_mutes_cache: Cache<String, HashSet<String>> = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(1800)) // 30 min TTL
            .build();

        Self {
            thread_mutes_cache,
            db_pool,
        }
    }

    /// Check if user has muted this thread
    pub async fn is_thread_muted(&self, user_did: &str, thread_root_uri: &str) -> bool {
        // Check cache first
        if let Some(muted_threads) = self.thread_mutes_cache.get(user_did) {
            return muted_threads.contains(thread_root_uri);
        }

        // Load from database
        match self.load_muted_threads(user_did).await {
            Ok(muted_threads) => {
                let result = muted_threads.contains(thread_root_uri);
                // Cache the result
                self.thread_mutes_cache
                    .insert(user_did.to_string(), muted_threads)
                    .await;
                result
            }
            Err(e) => {
                error!("Failed to load muted threads for {}: {}", user_did, e);
                false
            }
        }
    }

    /// Load all muted thread URIs for a user
    async fn load_muted_threads(&self, user_did: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT thread_root_uri
            FROM thread_mutes
            WHERE user_did = $1
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch muted threads")?;

        Ok(rows
            .into_iter()
            .map(|row| row.thread_root_uri)
            .collect())
    }

    /// Mute a thread for a user
    pub async fn mute_thread(&self, user_did: &str, thread_root_uri: &str) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO thread_mutes (user_did, thread_root_uri)
            VALUES ($1, $2)
            ON CONFLICT (user_did, thread_root_uri) DO NOTHING
            "#,
            user_did,
            thread_root_uri
        )
        .execute(&self.db_pool)
        .await
        .context("Failed to insert thread mute")?;

        // Invalidate cache
        self.thread_mutes_cache.invalidate(user_did).await;

        info!("User {} muted thread {}", user_did, thread_root_uri);
        Ok(())
    }

    /// Unmute a thread for a user
    pub async fn unmute_thread(&self, user_did: &str, thread_root_uri: &str) -> Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM thread_mutes
            WHERE user_did = $1 AND thread_root_uri = $2
            "#,
            user_did,
            thread_root_uri
        )
        .execute(&self.db_pool)
        .await
        .context("Failed to delete thread mute")?;

        // Invalidate cache
        self.thread_mutes_cache.invalidate(user_did).await;

        info!("User {} unmuted thread {}", user_did, thread_root_uri);
        Ok(())
    }

    /// Invalidate cache for a user
    pub async fn invalidate_user_cache(&self, user_did: &str) {
        self.thread_mutes_cache.invalidate(user_did).await;
    }

    /// Get cache statistics for monitoring
    pub fn get_cache_stats(&self) -> u64 {
        self.thread_mutes_cache.entry_count()
    }

    /// Get all muted threads for a user (for debugging/API)
    pub async fn get_muted_threads(&self, user_did: &str) -> Result<Vec<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT thread_root_uri
            FROM thread_mutes
            WHERE user_did = $1
            ORDER BY created_at DESC
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch muted threads")?;

        Ok(rows.into_iter().map(|row| row.thread_root_uri).collect())
    }
}
