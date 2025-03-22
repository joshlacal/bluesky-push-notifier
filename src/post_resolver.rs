// post_resolver.rs
use anyhow::{Context, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// API response structures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPostsResponse {
    pub posts: Vec<PostView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostView {
    pub uri: String,
    pub cid: String,
    pub author: Author,
    pub record: PostRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub did: String,
    pub handle: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRecord {
    pub text: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

// Cache entry with expiration
#[derive(Clone)]
struct CachedPostInfo {
    uri: String,
    text: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct PostResolver {
    http_client: HttpClient,
    memory_cache: Arc<RwLock<HashMap<String, CachedPostInfo>>>,
    db_pool: Pool<Postgres>,
    ttl: Duration,
    bsky_service_url: String,
}

impl PostResolver {
    pub fn new(db_pool: Pool<Postgres>, ttl_minutes: u64, bsky_service_url: String) -> Self {
        Self {
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            memory_cache: Arc::new(RwLock::new(HashMap::new())),
            db_pool,
            ttl: Duration::from_secs(ttl_minutes * 60),
            bsky_service_url,
        }
    }

    // Main method to get post content from URI
    pub async fn get_post_content(&self, uri: &str) -> Result<String> {
        // 1. Check memory cache first
        let content = self.get_from_memory_cache(uri).await;
        if let Some(content) = content {
            debug!(uri = %uri, "Post content found in memory cache");
            return Ok(content);
        }

        // 2. Check database cache
        let db_result = self.get_from_db_cache(uri).await?;
        if let Some((uri_str, text)) = db_result {
            // Update memory cache and return content
            self.update_memory_cache(uri_str, text.clone()).await;
            debug!(uri = %uri, "Post content found in database cache");
            return Ok(text);
        }

        // 3. Fetch from network
        info!(uri = %uri, "Fetching post content from network");
        let text = self.fetch_post_from_network(uri).await?;
        
        // 4. Update both caches
        self.update_caches(uri.to_string(), text.clone()).await?;
        
        Ok(text)
    }

    // Check memory cache for a post URI
    async fn get_from_memory_cache(&self, uri: &str) -> Option<String> {
        let cache = self.memory_cache.read().await;
        if let Some(cached) = cache.get(uri) {
            if cached.expires_at > Instant::now() {
                return Some(cached.text.clone());
            }
        }
        None
    }

    // Check database cache for a post URI
    async fn get_from_db_cache(&self, uri: &str) -> Result<Option<(String, String)>> {
        let row = sqlx::query!(
            r#"
            SELECT uri, text, expires_at 
            FROM post_cache 
            WHERE uri = $1 AND expires_at > NOW()
            "#,
            uri
        )
        .fetch_optional(&self.db_pool)
        .await?;
        
        if let Some(row) = row {
            return Ok(Some((row.uri, row.text)));
        }
        
        Ok(None)
    }

    // Update memory cache with new post info
    async fn update_memory_cache(&self, uri: String, text: String) {
        let mut cache = self.memory_cache.write().await;
        cache.insert(uri.clone(), CachedPostInfo {
            uri,
            text,
            expires_at: Instant::now() + self.ttl,
        });
    }

    // Update both caches with new post info
    async fn update_caches(&self, uri: String, text: String) -> Result<()> {
        // Update database cache
        let expires_at = time::OffsetDateTime::now_utc() + time::Duration::minutes(60);
            
        sqlx::query!(
            r#"
            INSERT INTO post_cache (uri, text, expires_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (uri) DO UPDATE
            SET text = $2, expires_at = $3
            "#,
            uri.as_str(),
            &text,
            expires_at
        )
        .execute(&self.db_pool)
        .await?;
        
        // Update memory cache
        self.update_memory_cache(uri, text).await;
        
        Ok(())
    }

    // Fetch post content from the Bluesky API
    async fn fetch_post_from_network(&self, uri: &str) -> Result<String> {
        // Construct API endpoint for fetching a single post
        let url = format!("https://{}/xrpc/app.bsky.feed.getPosts", self.bsky_service_url);
        
        let response = self.http_client.get(&url)
            .query(&[("uris", uri)])
            .send()
            .await
            .with_context(|| format!("Failed to fetch post content for URI: {}", uri))?;
            
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to fetch post, status: {}", 
                response.status()
            ));
        }
        
        let post_data: GetPostsResponse = response.json()
            .await
            .with_context(|| format!("Failed to parse post data for URI: {}", uri))?;
            
        // Get post text content
        let post_text = post_data.posts.get(0)
        .ok_or_else(|| anyhow::anyhow!("No posts returned for URI: {}", uri))?
        .record.text.clone();
            
        // Truncate if needed - don't want notification body to be too long
        let truncated_text = if post_text.len() > 140 {
            format!("{}...", &post_text[..137])
        } else {
            post_text
        };
        
        Ok(truncated_text)
    }

    // Cleanup expired entries
    pub async fn cleanup_expired(&self) -> Result<usize> {
        // Clean memory cache
        let mut memory_cleaned = 0;
        {
            let mut cache = self.memory_cache.write().await;
            let now = Instant::now();
            cache.retain(|_, v| {
                let keep = v.expires_at > now;
                if !keep {
                    memory_cleaned += 1;
                }
                keep
            });
        }
        
        // Clean database cache
        let db_result = sqlx::query!(
            "DELETE FROM post_cache WHERE expires_at <= NOW() RETURNING uri"
        )
        .fetch_all(&self.db_pool)
        .await?;
        
        let db_cleaned = db_result.len();
        
        info!(
            memory_cleaned = %memory_cleaned,
            db_cleaned = %db_cleaned,
            "Cleaned expired post cache entries"
        );
        
        Ok(memory_cleaned + db_cleaned)
    }
}