use anyhow::{Context, Result};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationList {
    pub uri: String,
    pub purpose: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListMember {
    pub subject: String,
}

pub struct ModerationListManager {
    // Cache: user_did -> set of blocked_dids from all their subscribed block lists
    block_lists_cache: Cache<String, HashSet<String>>,
    // Cache: user_did -> set of muted_dids from all their subscribed mute lists
    mute_lists_cache: Cache<String, HashSet<String>>,
    db_pool: Pool<Postgres>,
}

impl ModerationListManager {
    pub fn new(db_pool: Pool<Postgres>) -> Self {
        let block_lists_cache: Cache<String, HashSet<String>> = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(1800)) // 30 min TTL
            .build();

        let mute_lists_cache: Cache<String, HashSet<String>> = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(1800)) // 30 min TTL
            .build();

        Self {
            block_lists_cache,
            mute_lists_cache,
            db_pool,
        }
    }

    /// Check if target_did is in any of user's subscribed block lists
    pub async fn is_in_block_list(&self, user_did: &str, target_did: &str) -> bool {
        // Check cache first
        if let Some(blocked_dids) = self.block_lists_cache.get(user_did) {
            return blocked_dids.contains(target_did);
        }

        // Load from database
        match self.load_block_list_members(user_did).await {
            Ok(blocked_dids) => {
                let result = blocked_dids.contains(target_did);
                // Cache the result
                self.block_lists_cache
                    .insert(user_did.to_string(), blocked_dids)
                    .await;
                result
            }
            Err(e) => {
                error!("Failed to load block list members for {}: {}", user_did, e);
                false
            }
        }
    }

    /// Check if target_did is in any of user's subscribed mute lists
    pub async fn is_in_mute_list(&self, user_did: &str, target_did: &str) -> bool {
        // Check cache first
        if let Some(muted_dids) = self.mute_lists_cache.get(user_did) {
            return muted_dids.contains(target_did);
        }

        // Load from database
        match self.load_mute_list_members(user_did).await {
            Ok(muted_dids) => {
                let result = muted_dids.contains(target_did);
                // Cache the result
                self.mute_lists_cache
                    .insert(user_did.to_string(), muted_dids)
                    .await;
                result
            }
            Err(e) => {
                error!("Failed to load mute list members for {}: {}", user_did, e);
                false
            }
        }
    }

    /// Load all DIDs from user's subscribed block lists (modlist purpose)
    async fn load_block_list_members(&self, user_did: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT m.subject_did
            FROM moderation_list_members m
            INNER JOIN moderation_list_subscriptions s ON m.list_uri = s.list_uri
            WHERE s.user_did = $1 AND s.list_purpose = 'modlist'
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch block list members")?;

        Ok(rows.into_iter().map(|row| row.subject_did).collect())
    }

    /// Load all DIDs from user's subscribed mute lists (curatelist purpose)
    async fn load_mute_list_members(&self, user_did: &str) -> Result<HashSet<String>> {
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT m.subject_did
            FROM moderation_list_members m
            INNER JOIN moderation_list_subscriptions s ON m.list_uri = s.list_uri
            WHERE s.user_did = $1 AND s.list_purpose = 'curatelist'
            "#,
            user_did
        )
        .fetch_all(&self.db_pool)
        .await
        .context("Failed to fetch mute list members")?;

        Ok(rows.into_iter().map(|row| row.subject_did).collect())
    }

    /// Sync user's moderation list subscriptions and members
    pub async fn sync_moderation_lists(
        &self,
        user_did: &str,
        lists: Vec<ModerationList>,
        at_proto_client: &reqwest::Client,
        pds_url: &str,
        access_token: &str,
    ) -> Result<()> {
        info!("Syncing {} moderation lists for user {}", lists.len(), user_did);

        // Start transaction
        let mut tx = self.db_pool.begin().await?;

        // Delete existing subscriptions for this user
        sqlx::query!(
            "DELETE FROM moderation_list_subscriptions WHERE user_did = $1",
            user_did
        )
        .execute(&mut *tx)
        .await?;

        // Insert new subscriptions and fetch members
        for list in lists {
            // Insert subscription
            sqlx::query!(
                r#"
                INSERT INTO moderation_list_subscriptions 
                (user_did, list_uri, list_purpose, list_name, last_synced_at)
                VALUES ($1, $2, $3, $4, NOW())
                "#,
                user_did,
                list.uri,
                list.purpose,
                list.name
            )
            .execute(&mut *tx)
            .await?;

            // Fetch list members from AT Protocol
            match self
                .fetch_list_members(&list.uri, at_proto_client, pds_url, access_token)
                .await
            {
                Ok(members) => {
                    debug!("Fetched {} members for list {}", members.len(), list.uri);

                    // Delete existing members for this list
                    sqlx::query!("DELETE FROM moderation_list_members WHERE list_uri = $1", list.uri)
                        .execute(&mut *tx)
                        .await?;

                    // Insert new members
                    for member in members {
                        sqlx::query!(
                            r#"
                            INSERT INTO moderation_list_members (list_uri, subject_did)
                            VALUES ($1, $2)
                            ON CONFLICT (list_uri, subject_did) DO NOTHING
                            "#,
                            list.uri,
                            member.subject
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch members for list {}: {}", list.uri, e);
                    // Continue with other lists even if one fails
                }
            }
        }

        // Commit transaction
        tx.commit().await?;

        // Invalidate caches for this user
        self.block_lists_cache.invalidate(user_did).await;
        self.mute_lists_cache.invalidate(user_did).await;

        info!("Successfully synced moderation lists for user {}", user_did);
        Ok(())
    }

    /// Fetch members of a moderation list from AT Protocol
    async fn fetch_list_members(
        &self,
        list_uri: &str,
        client: &reqwest::Client,
        pds_url: &str,
        access_token: &str,
    ) -> Result<Vec<ListMember>> {
        let url = format!("{}/xrpc/app.bsky.graph.getList", pds_url);
        
        let response = client
            .get(&url)
            .query(&[("list", list_uri), ("limit", "100")])
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await
            .context("Failed to fetch list members")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch list: HTTP {}", response.status());
        }

        #[derive(Deserialize)]
        struct ListResponse {
            items: Vec<ListItem>,
        }

        #[derive(Deserialize)]
        struct ListItem {
            subject: SubjectDid,
        }

        #[derive(Deserialize)]
        struct SubjectDid {
            did: String,
        }

        let list_response: ListResponse = response
            .json()
            .await
            .context("Failed to parse list response")?;

        Ok(list_response
            .items
            .into_iter()
            .map(|item| ListMember {
                subject: item.subject.did,
            })
            .collect())
    }

    /// Invalidate caches for a user (call after sync)
    pub async fn invalidate_user_cache(&self, user_did: &str) {
        self.block_lists_cache.invalidate(user_did).await;
        self.mute_lists_cache.invalidate(user_did).await;
    }

    /// Get cache statistics for monitoring
    pub fn get_cache_stats(&self) -> (u64, u64) {
        (
            self.block_lists_cache.entry_count(),
            self.mute_lists_cache.entry_count(),
        )
    }
}
