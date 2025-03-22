// post_resolver.rs
use anyhow::{Result};
use circuit_breaker::CircuitBreaker;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres, types::time};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, oneshot};
use tracing::{debug, info, warn};
use ::time::Duration as TimeDuration;

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
    api_circuit_breaker: Arc<RwLock<CircuitBreaker>>,
    request_queue: Arc<Mutex<HashMap<String, oneshot::Sender<Result<String>>>>>,
    trigger_send: Arc<tokio::sync::Notify>,
}

// Define our own CircuitBreakerConfig since it's not provided by the library
#[derive(Debug, Clone)]
struct CircuitBreakerConfig {
    failure_threshold: u32,
    success_threshold: u32,
    open_duration: Duration,
}

impl PostResolver {
    pub fn new(db_pool: Pool<Postgres>, ttl_minutes: u64, bsky_service_url: String) -> Self {
        // Configure circuit breaker with appropriate settings
        let cb_config = CircuitBreakerConfig {
            failure_threshold: 5,         // Trip after 5 failures
            success_threshold: 2,         // Require 2 successful calls to reset
            open_duration: Duration::from_secs(30), // Stay open for 30 seconds
        };
        
        let request_queue = Arc::new(Mutex::new(HashMap::new()));
        let trigger_send = Arc::new(tokio::sync::Notify::new());
        
        // Create circuit breaker using the version's API
        let circuit_breaker = CircuitBreaker::new(
            cb_config.failure_threshold,
            cb_config.open_duration
        );
        
        let resolver = Self {
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            memory_cache: Arc::new(RwLock::new(HashMap::new())),
            db_pool,
            ttl: Duration::from_secs(ttl_minutes * 60),
            bsky_service_url,
            api_circuit_breaker: Arc::new(RwLock::new(circuit_breaker)),
            request_queue,
            trigger_send,
        };
        
        // Start background task for batch processing
        let resolver_clone = resolver.clone();
        tokio::spawn(async move {
            resolver_clone.run_request_processor().await;
        });
        
        resolver
    }

    // Main method to get post content from URI
    pub async fn get_post_content(&self, uri: &str) -> Result<String> {
        // Create timer to measure fetching time
        let timer = std::time::Instant::now();
        
        // 1. Check memory cache first
        let content = self.get_from_memory_cache(uri).await;
        if let Some(content) = content {
            // Record cache hit metric
            crate::metrics::POST_CACHE_HITS.inc();
            let elapsed = timer.elapsed().as_secs_f64();
            crate::metrics::POST_FETCH_TIME.observe(elapsed);
            
            debug!(uri = %uri, "Post content found in memory cache");
            return Ok(content);
        }

        // 2. Check database cache
        let db_result = self.get_from_db_cache(uri).await?;
        if let Some((uri_str, text)) = db_result {
            // Update memory cache and return content
            self.update_memory_cache(uri_str, text.clone()).await;
            // Record cache hit metric
            crate::metrics::POST_CACHE_HITS.inc();
            let elapsed = timer.elapsed().as_secs_f64();
            crate::metrics::POST_FETCH_TIME.observe(elapsed);
            
            debug!(uri = %uri, "Post content found in database cache");
            return Ok(text);
        }

        // 3. Record cache miss metric
        crate::metrics::POST_CACHE_MISSES.inc();
        
        // 4. Queue request for batch processing
        info!(uri = %uri, "Queuing post content fetch for batch processing");
        let (sender, receiver) = oneshot::channel();
        {
            let mut queue = self.request_queue.lock().await;
            queue.insert(uri.to_string(), sender);
        }
        
        // Notify the processor that we have a new request
        self.trigger_send.notify_one();
        
        // Wait for the result with a timeout to ensure low latency
        match tokio::time::timeout(Duration::from_millis(150), receiver).await {
            Ok(result) => {
                match result {
                    Ok(text) => {
                        // Record fetch time
                        let elapsed = timer.elapsed().as_secs_f64();
                        crate::metrics::POST_FETCH_TIME.observe(elapsed);
                        
                        debug!(uri = %uri, "Received post content from batch processor");
                        text
                    }
                    Err(_) => {
                        warn!(uri = %uri, "Batch processor disappeared, falling back to direct fetch");
                        self.fetch_and_cache_individual(uri, timer).await
                    }
                }
            },
            Err(_) => {
                // Timeout occurred, make an individual request instead
                warn!(uri = %uri, "Batch processing timeout, falling back to direct fetch");
                self.fetch_and_cache_individual(uri, timer).await
            }
        }
    }
    
    // Helper to fetch and cache an individual post (fallback)
    async fn fetch_and_cache_individual(&self, uri: &str, timer: Instant) -> Result<String> {
        match self.fetch_post_from_network_individual(uri).await {
            Ok(text) => {
                // Update both caches asynchronously
                let uri_clone = uri.to_string();
                let text_clone = text.clone();
                let self_clone = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = self_clone.update_caches(uri_clone, text_clone).await {
                        warn!("Failed to update caches: {}", e);
                    }
                });
                
                // Record fetch time
                let elapsed = timer.elapsed().as_secs_f64();
                crate::metrics::POST_FETCH_TIME.observe(elapsed);
                
                Ok(text)
            },
            Err(e) => Err(e)
        }
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
        let expires_at = time::OffsetDateTime::now_utc() + TimeDuration::minutes(60);
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

    // New method to fetch multiple posts at once
    async fn fetch_posts_batch(&self, uris: &[String]) -> Result<HashMap<String, String>> {
        // Check if circuit breaker is open using the correct API
        let circuit_breaker = self.api_circuit_breaker.read().await;
        // The crate uses state() which returns an enum, match on the enum type
        let is_open = match circuit_breaker.state() {
            circuit_breaker::CircuitState::Open => true,
            _ => false,
        };
        
        if is_open {
            warn!("Circuit breaker open, returning fallback content for batch request");
            let mut results = HashMap::new();
            for uri in uris {
                results.insert(uri.clone(), "Content temporarily unavailable".to_string());
            }
            return Ok(results);
        }
        drop(circuit_breaker); // Release read lock before we need to write
        
        // Create batch timer for metrics
        let batch_timer = std::time::Instant::now();
        
        // Construct URL for batch request
        let url = format!("https://{}/xrpc/app.bsky.feed.getPosts", self.bsky_service_url);
        
        // Create query parameter with multiple URIs - one parameter per URI
        let query_params = uris.iter().map(|uri| ("uris", uri.as_str())).collect::<Vec<_>>();
        
        // Make the API request
        let response_result = self.http_client.get(&url)
            .query(&query_params)
            .send()
            .await;
            
        match response_result {
            Ok(response) => {
                if response.status().is_success() {
                    // Record success with circuit breaker
                    self.api_circuit_breaker.write().await.handle_success();
                    
                    match response.json::<GetPostsResponse>().await {
                        Ok(post_data) => {
                            let mut results = HashMap::new();
                            
                            // Process each post in the response
                            for post in post_data.posts {
                                // Extract and truncate text
                                let text = if post.record.text.len() > 140 {
                                    format!("{}...", &post.record.text[..137])
                                } else {
                                    post.record.text
                                };
                                
                                // Add to results
                                results.insert(post.uri, text);
                            }
                            
                            // Record batch metrics
                            let elapsed = batch_timer.elapsed().as_secs_f64();
                            info!(
                                "Batch request for {} URIs completed in {:.2}s, received {} posts",
                                uris.len(), elapsed, results.len()
                            );
                            
                            Ok(results)
                        },
                        Err(e) => {
                            // Record failure with circuit breaker
                            self.api_circuit_breaker.write().await.handle_failure();
                            Err(anyhow::anyhow!("Failed to parse batch post data: {}", e))
                        }
                    }
                } else {
                    // Record failure with circuit breaker
                    self.api_circuit_breaker.write().await.handle_failure();
                    Err(anyhow::anyhow!(
                        "Failed to fetch batch posts, status: {}", 
                        response.status()
                    ))
                }
            },
            Err(e) => {
                // Record failure with circuit breaker
                self.api_circuit_breaker.write().await.handle_failure();
                Err(anyhow::anyhow!("Failed to fetch batch post content: {}", e))
            }
        }
    }
    
    // Individual post fetching as fallback (renamed from original fetch_post_from_network)
    async fn fetch_post_from_network_individual(&self, uri: &str) -> Result<String> {
        // Check if circuit breaker is open
        let circuit_breaker = self.api_circuit_breaker.read().await;
        let is_open = match circuit_breaker.state() {
            circuit_breaker::CircuitState::Open => true,
            _ => false,
        };
        
        if is_open {
            warn!("Circuit breaker open, returning fallback content for {}", uri);
            return Ok("Content temporarily unavailable".to_string());
        }
        drop(circuit_breaker); // Release read lock before we need to write
        
        // Construct API endpoint for fetching a single post
        let url = format!("https://{}/xrpc/app.bsky.feed.getPosts", self.bsky_service_url);
        
        // Attempt to make the API request
        let response_result = self.http_client.get(&url)
            .query(&[("uris", uri)])
            .send()
            .await;
            
        match response_result {
            Ok(response) => {
                if response.status().is_success() {
                    // Record success with circuit breaker
                    self.api_circuit_breaker.write().await.handle_success();
                    
                    match response.json::<GetPostsResponse>().await {
                        Ok(post_data) => {
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
                        },
                        Err(e) => {
                            // Record failure with circuit breaker
                            self.api_circuit_breaker.write().await.handle_failure();
                            Err(anyhow::anyhow!("Failed to parse post data for URI {}: {}", uri, e))
                        }
                    }
                } else {
                    // Record failure with circuit breaker
                    self.api_circuit_breaker.write().await.handle_failure();
                    Err(anyhow::anyhow!(
                        "Failed to fetch post, status: {}", 
                        response.status()
                    ))
                }
            },
            Err(e) => {
                // Record failure with circuit breaker
                self.api_circuit_breaker.write().await.handle_failure();
                Err(anyhow::anyhow!("Failed to fetch post content for URI {}: {}", uri, e))
            }
        }
    }

    // Fix the original fetch_post_from_network method to use fetch_post_from_network_individual
    async fn fetch_post_from_network(&self, uri: &str) -> Result<String> {
        self.fetch_post_from_network_individual(uri).await
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

    // Background task to process batched requests
    async fn run_request_processor(&self) {
        let max_batch_size = 25; // API limit or reasonable maximum
        let max_wait_time = Duration::from_millis(50); // Maximum latency we're willing to accept
        
        loop {
            // Wait for either new requests or timeout
            tokio::select! {
                _ = self.trigger_send.notified() => {
                    // Continue immediately to process
                },
                _ = tokio::time::sleep(max_wait_time) => {
                    // Check if we have any pending requests
                    let queue_len = {
                        let queue = self.request_queue.lock().await;
                        queue.len()
                    };
                    
                    if queue_len == 0 {
                        continue;
                    }
                }
            }
            
            // Extract up to max_batch_size requests from the queue
            let requests = {
                let mut queue = self.request_queue.lock().await;
                if queue.is_empty() {
                    continue;
                }
                
                // Take all requests up to max_batch_size
                let mut requests = HashMap::new();
                
                // Use drain_filter to avoid borrowing issues
                let keys: Vec<String> = queue.keys().cloned().take(max_batch_size).collect();
                for key in keys {
                    if let Some(sender) = queue.remove(&key) {
                        requests.insert(key, sender);
                    }
                }
                
                requests
            };
            
            if requests.is_empty() {
                continue;
            }
            
            // Record batch size metric
            let batch_size = requests.len() as f64;
            crate::metrics::POST_BATCH_SIZE.observe(batch_size);
            
            // Log batch size 
            info!("Processing batch of {} post requests", batch_size);
            
            // Start batch latency timer
            let batch_timer = std::time::Instant::now();
            
            // Make a batch request
            let uris: Vec<String> = requests.keys().cloned().collect();
            match self.fetch_posts_batch(&uris).await {
                Ok(results) => {
                    // Measure and record batch latency
                    let elapsed = batch_timer.elapsed().as_secs_f64();
                    crate::metrics::POST_BATCH_LATENCY.observe(elapsed);
                    
                    info!(
                        "Batch request for {} URIs completed in {:.2}s, received {} posts",
                        batch_size, elapsed, results.len()
                    );
                    
                    // Update caches for all results asynchronously
                    let self_clone = self.clone();
                    let results_clone = results.clone();
                    tokio::spawn(async move {
                        for (uri, text) in &results_clone {
                            if let Err(e) = self_clone.update_caches(uri.clone(), text.clone()).await {
                                warn!("Failed to update cache for {}: {}", uri, e);
                            }
                        }
                    });
                    
                    // Respond to all requesters - move senders to avoid borrowing issues
                    for (uri, sender) in requests {
                        if let Some(text) = results.get(&uri) {
                            let _ = sender.send(Ok(text.clone()));
                        } else {
                            // URI wasn't found in results - do individual request as fallback
                            let self_clone = self.clone();
                            let uri_clone = uri.clone();
                            tokio::spawn(async move {
                                match self_clone.fetch_post_from_network_individual(&uri_clone).await {
                                    Ok(text) => {
                                        // Also update caches
                                        let _ = self_clone.update_caches(uri_clone.clone(), text.clone()).await;
                                        let _ = sender.send(Ok(text));
                                    },
                                    Err(e) => {
                                        let _ = sender.send(Err(e));
                                    }
                                }
                            });
                        }
                    }
                },
                Err(e) => {
                    // Record batch latency even for errors
                    let elapsed = batch_timer.elapsed().as_secs_f64();
                    crate::metrics::POST_BATCH_LATENCY.observe(elapsed);
                    
                    warn!("Batch request failed: {}", e);
                    
                    // Fall back to individual requests for all items
                    for (uri, sender) in requests {
                        let self_clone = self.clone();
                        let uri_clone = uri.clone();
                        tokio::spawn(async move {
                            match self_clone.fetch_post_from_network_individual(&uri_clone).await {
                                Ok(text) => {
                                    // Also update caches
                                    let _ = self_clone.update_caches(uri_clone.clone(), text.clone()).await;
                                    let _ = sender.send(Ok(text));
                                },
                                Err(e) => {
                                    let _ = sender.send(Err(e));
                                }
                            }
                        });
                    }
                }
            }
        }
    }
}