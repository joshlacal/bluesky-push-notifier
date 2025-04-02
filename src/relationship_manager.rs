use anyhow::{Context, Result};
use moka::future::Cache;
use sqlx::{Pool, Postgres};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::models::UserDevice;

pub struct RelationshipManager {
    // Moka caches
    mutes_cache: Cache<String, HashSet<String>>, // user_did -> set of muted_dids
    blocks_cache: Cache<String, HashSet<String>>, // user_did -> set of blocked_dids
    db_pool: Pool<Postgres>,
}

impl RelationshipManager {
    pub fn new(db_pool: Pool<Postgres>) -> Self {
        // Create caches with reasonable TTL and size limits
        let mutes_cache: Cache<String, HashSet<String>> = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(3600)) // 1 hour TTL
            .build();

        let blocks_cache: Cache<String, HashSet<String>> = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(3600)) // 1 hour TTL
            .build();

        Self {
            mutes_cache,
            blocks_cache,
            db_pool,
        }
    }

    // Check if user_did has muted target_did
    pub async fn is_muted(&self, user_did: &str, target_did: &str) -> bool {
        if let Some(mutes) = self.mutes_cache.get(user_did) {
            return mutes.contains(target_did);
        }

        // Not in cache, load from database
        match self.load_mutes_for_user(user_did).await {
            Ok(mutes) => mutes.contains(target_did),
            Err(e) => {
                error!("Failed to load mutes for {}: {}", user_did, e);
                false
            }
        }
    }

    // Check if user_did has blocked target_did
    pub async fn is_blocked(&self, user_did: &str, target_did: &str) -> bool {
        if let Some(blocks) = self.blocks_cache.get(user_did) {
            return blocks.contains(target_did);
        }

        // Not in cache, load from database
        match self.load_blocks_for_user(user_did).await {
            Ok(blocks) => blocks.contains(target_did),
            Err(e) => {
                error!("Failed to load blocks for {}: {}", user_did, e);
                false
            }
        }
    }

    // Load mutes for a user from DB and update cache
    async fn load_mutes_for_user(&self, user_did: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT muted_did FROM user_mutes 
            WHERE user_did = $1
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch user mutes")?;

        let mutes: HashSet<String> = rows.into_iter().map(|row| row.muted_did).collect();

        // Update cache
        self.mutes_cache
            .insert(user_did.to_string(), mutes.clone())
            .await;

        Ok(mutes)
    }

    // Load blocks for a user from DB and update cache
    async fn load_blocks_for_user(&self, user_did: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT blocked_did FROM user_blocks 
            WHERE user_did = $1
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch user blocks")?;

        let blocks: HashSet<String> = rows.into_iter().map(|row| row.blocked_did).collect();

        // Update cache
        self.blocks_cache
            .insert(user_did.to_string(), blocks.clone())
            .await;

        Ok(blocks)
    }

    // Authenticate device token before updating relationships
    async fn authenticate_device(&self, did: &str, device_token: &str) -> Result<UserDevice> {
        let device = sqlx::query_as!(
            UserDevice,
            r#"
            SELECT id, did, device_token, created_at, updated_at
            FROM user_devices
            WHERE did = $1 AND device_token = $2
            "#,
            did,
            device_token
        )
        .fetch_optional(&self.db_pool)
        .await
        .context("Error querying device")?;

        match device {
            Some(d) => Ok(d),
            None => Err(anyhow::anyhow!("Invalid device token for DID")),
        }
    }

    // Update user mutes from client - with authentication
    pub async fn update_user_mutes(
        &self,
        user_did: &str,
        device_token: &str,
        muted_dids: Vec<String>,
    ) -> Result<()> {
        // Authenticate the device
        let device = self.authenticate_device(user_did, device_token).await?;

        // Start a transaction
        let mut tx = self.db_pool.begin().await?;

        // Clear existing mutes
        sqlx::query!("DELETE FROM user_mutes WHERE user_did = $1", user_did)
            .execute(&mut *tx)
            .await
            .context("Failed to delete existing mutes")?;

        // Insert new mutes
        for muted_did in &muted_dids {
            sqlx::query!(
                r#"
                INSERT INTO user_mutes (user_did, muted_did)
                VALUES ($1, $2)
                "#,
                user_did,
                muted_did
            )
            .execute(&mut *tx)
            .await
            .context("Failed to insert mute relationship")?;
        }

        // Record audit log
        sqlx::query!(
            r#"
            INSERT INTO relationship_audit_log (user_did, device_token, action, details)
            VALUES ($1, $2, $3, $4)
            "#,
            user_did,
            device_token,
            "update_mutes",
            serde_json::to_value(&muted_dids)?
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record audit log")?;

        // Commit transaction
        tx.commit()
            .await
            .context("Failed to commit mutes transaction")?;

        // Update cache
        let mute_set: HashSet<String> = muted_dids.into_iter().collect();
        self.mutes_cache
            .insert(user_did.to_string(), mute_set)
            .await;

        info!(user_did = %user_did, "Updated user mutes");
        Ok(())
    }

    // Update user blocks from client - with authentication
    pub async fn update_user_blocks(
        &self,
        user_did: &str,
        device_token: &str,
        blocked_dids: Vec<String>,
    ) -> Result<()> {
        // Authenticate the device
        let device = self.authenticate_device(user_did, device_token).await?;

        // Start a transaction
        let mut tx = self.db_pool.begin().await?;

        // Clear existing blocks
        sqlx::query!("DELETE FROM user_blocks WHERE user_did = $1", user_did)
            .execute(&mut *tx)
            .await
            .context("Failed to delete existing blocks")?;

        // Insert new blocks
        for blocked_did in &blocked_dids {
            sqlx::query!(
                r#"
                INSERT INTO user_blocks (user_did, blocked_did)
                VALUES ($1, $2)
                "#,
                user_did,
                blocked_did
            )
            .execute(&mut *tx)
            .await
            .context("Failed to insert block relationship")?;
        }

        // Record audit log
        sqlx::query!(
            r#"
            INSERT INTO relationship_audit_log (user_did, device_token, action, details)
            VALUES ($1, $2, $3, $4)
            "#,
            user_did,
            device_token,
            "update_blocks",
            serde_json::to_value(&blocked_dids)?
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record audit log")?;

        // Commit transaction
        tx.commit()
            .await
            .context("Failed to commit blocks transaction")?;

        // Update cache
        let block_set: HashSet<String> = blocked_dids.into_iter().collect();
        self.blocks_cache
            .insert(user_did.to_string(), block_set)
            .await;

        info!(user_did = %user_did, "Updated user blocks");
        Ok(())
    }

    // Update both mutes and blocks in a single batch operation - with authentication
    pub async fn update_relationships_batch(
        &self,
        user_did: &str,
        device_token: &str,
        mutes: Vec<String>,
        blocks: Vec<String>,
    ) -> Result<()> {
        // Authenticate first
        let device = self.authenticate_device(user_did, device_token).await?;

        // Start a transaction for the entire batch
        let mut tx = self.db_pool.begin().await?;

        // Clear existing relationships
        sqlx::query!("DELETE FROM user_mutes WHERE user_did = $1", user_did)
            .execute(&mut *tx)
            .await
            .context("Failed to delete existing mutes")?;

        sqlx::query!("DELETE FROM user_blocks WHERE user_did = $1", user_did)
            .execute(&mut *tx)
            .await
            .context("Failed to delete existing blocks")?;

        // Use batch inserts for better performance
        if !mutes.is_empty() {
            // Use parameterized queries with sqlx to safely handle multiple inserts
            let mut query_builder =
                String::from("INSERT INTO user_mutes (user_did, muted_did) VALUES ");
            let mut params = Vec::new();
            let mut param_idx = 1;

            for (i, muted_did) in mutes.iter().enumerate() {
                if i > 0 {
                    query_builder.push_str(", ");
                }
                query_builder.push_str(&format!("(${},${})", param_idx, param_idx + 1));
                params.push(user_did.to_string());
                params.push(muted_did.clone());
                param_idx += 2;
            }

            let query = sqlx::query(&query_builder);
            // Apply all parameters
            let query = params.iter().fold(query, |q, param| q.bind(param));

            query
                .execute(&mut *tx)
                .await
                .context("Failed to batch insert mute relationships")?;
        }

        // Similar batch approach for blocks
        if !blocks.is_empty() {
            let mut query_builder =
                String::from("INSERT INTO user_blocks (user_did, blocked_did) VALUES ");
            let mut params = Vec::new();
            let mut param_idx = 1;

            for (i, blocked_did) in blocks.iter().enumerate() {
                if i > 0 {
                    query_builder.push_str(", ");
                }
                query_builder.push_str(&format!("(${},${})", param_idx, param_idx + 1));
                params.push(user_did.to_string());
                params.push(blocked_did.clone());
                param_idx += 2;
            }

            let query = sqlx::query(&query_builder);
            // Apply all parameters
            let query = params.iter().fold(query, |q, param| q.bind(param));

            query
                .execute(&mut *tx)
                .await
                .context("Failed to batch insert block relationships")?;
        }

        // Record audit log with counts rather than full lists to reduce storage
        let combined_details = serde_json::json!({
            "mutes_count": mutes.len(),
            "blocks_count": blocks.len(),
            "timestamp": chrono::Utc::now().to_rfc3339()
        });

        sqlx::query!(
            r#"
            INSERT INTO relationship_audit_log (user_did, device_token, action, details)
            VALUES ($1, $2, $3, $4)
            "#,
            user_did,
            device_token,
            "update_relationships_batch",
            combined_details
        )
        .execute(&mut *tx)
        .await
        .context("Failed to record audit log")?;

        // Commit the transaction
        tx.commit()
            .await
            .context("Failed to commit relationship batch transaction")?;

        // Update caches
        let mute_set: HashSet<String> = mutes.into_iter().collect();
        let block_set: HashSet<String> = blocks.into_iter().collect();

        self.mutes_cache
            .insert(user_did.to_string(), mute_set)
            .await;
        self.blocks_cache
            .insert(user_did.to_string(), block_set)
            .await;

        info!(user_did = %user_did, "Updated user relationships in batch");
        Ok(())
    }

    // Invalidate cache entries for maintenance
    pub async fn invalidate_cache(&self, user_did: &str) {
        self.mutes_cache.invalidate(user_did).await;
        self.blocks_cache.invalidate(user_did).await;
        debug!(user_did = %user_did, "Invalidated relationship caches");
    }

    // Run periodic cache maintenance
    pub async fn run_cache_maintenance(&self) -> Result<()> {
        info!("Running relationship cache maintenance");

        // Get all DIDs with relationships
        let mute_dids = sqlx::query!(r#"SELECT DISTINCT user_did FROM user_mutes"#)
            .fetch_all(&self.db_pool)
            .await?
            .into_iter()
            .map(|row| row.user_did);

        let block_dids = sqlx::query!(r#"SELECT DISTINCT user_did FROM user_blocks"#)
            .fetch_all(&self.db_pool)
            .await?
            .into_iter()
            .map(|row| row.user_did);

        // Combine and deduplicate
        let mut all_dids: HashSet<String> = HashSet::new();
        all_dids.extend(mute_dids);
        all_dids.extend(block_dids);

        // Refresh cache for all DIDs
        let mut refresh_count = 0;
        for did in all_dids {
            let _ = self.load_mutes_for_user(&did).await;
            let _ = self.load_blocks_for_user(&did).await;
            refresh_count += 1;
        }

        info!("Refreshed relationship caches for {} users", refresh_count);
        Ok(())
    }
}
