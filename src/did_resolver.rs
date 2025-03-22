// did_resolver.rs
use anyhow::{Context, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn}; 

// Simplified DID Document structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DidDocument {
    pub id: String,
    #[serde(rename = "alsoKnownAs")]
    pub also_known_as: Option<Vec<String>>,
    pub service: Option<Vec<Service>>,
    // Add other fields as needed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    #[serde(rename = "type")]
    pub service_type: String,
    #[serde(rename = "serviceEndpoint")]
    pub service_endpoint: String,
}

// Cache entry with expiration
#[derive(Clone)]
struct CachedDidInfo {
    document: DidDocument,
    handle: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct DidResolver {
    http_client: HttpClient,
    memory_cache: Arc<RwLock<HashMap<String, CachedDidInfo>>>,
    db_pool: Pool<Postgres>,
    ttl: Duration,
}

impl DidResolver {
    pub fn new(db_pool: Pool<Postgres>, ttl_hours: u64) -> Self {
        Self {
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            memory_cache: Arc::new(RwLock::new(HashMap::new())),
            db_pool,
            ttl: Duration::from_secs(ttl_hours * 3600),
        }
    }

    // Main method to get a handle from a DID
    pub async fn get_handle(&self, did: &str) -> Result<String> {
        // 1. Check memory cache first
        let handle = self.get_from_memory_cache(did).await;
        if let Some(handle) = handle {
            debug!(did = %did, handle = %handle, "Handle found in memory cache");
            return Ok(handle);
        }

        // 2. Check database cache
        let db_result = self.get_from_db_cache(did).await?;
        if let Some((document, handle)) = db_result {
            // Update memory cache and return handle
            self.update_memory_cache(did.to_string(), document, handle.clone()).await;
            debug!(did = %did, handle = %handle, "Handle found in database cache");
            return Ok(handle);
        }

        // 3. Resolve via network
        info!(did = %did, "Resolving DID from network");
        let (document, handle) = self.resolve_did_network(did).await?;
        
        // 4. Update both caches
        self.update_caches(did.to_string(), document.clone(), handle.clone()).await?;
        
        Ok(handle)
    }

    // Check memory cache for a DID
    async fn get_from_memory_cache(&self, did: &str) -> Option<String> {
        let cache = self.memory_cache.read().await;
        if let Some(cached) = cache.get(did) {
            if cached.expires_at > Instant::now() {
                return Some(cached.handle.clone());
            }
        }
        None
    }

    // Check database cache for a DID
    async fn get_from_db_cache(&self, did: &str) -> Result<Option<(DidDocument, String)>> {
        let row = sqlx::query!(
            r#"
            SELECT document, handle, expires_at 
            FROM did_cache 
            WHERE did = $1 AND expires_at > NOW()
            "#,
            did
        )
        .fetch_optional(&self.db_pool)
        .await?;
        
        if let Some(row) = row {
            let document: DidDocument = serde_json::from_value(row.document)
                .with_context(|| "Failed to deserialize DID document from database")?;
            return Ok(Some((document, row.handle)));
        }
        
        Ok(None)
    }

    // Update memory cache with new DID info
    async fn update_memory_cache(&self, did: String, document: DidDocument, handle: String) {
        let mut cache = self.memory_cache.write().await;
        cache.insert(did, CachedDidInfo {
            document,
            handle,
            expires_at: Instant::now() + self.ttl,
        });
    }

    // Update both caches with new DID info
    async fn update_caches(&self, did: String, document: DidDocument, handle: String) -> Result<()> {
        // Update database cache
        let expires_at = time::OffsetDateTime::now_utc() + time::Duration::hours(24);
        let json_doc = serde_json::to_value(document.clone())
            .with_context(|| "Failed to serialize DID document")?;
            
        sqlx::query!(
            r#"
            INSERT INTO did_cache (did, document, handle, expires_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (did) DO UPDATE
            SET document = $2, handle = $3, expires_at = $4
            "#,
            did.as_str(),
            json_doc,
            &handle,
            expires_at
        )
        .execute(&self.db_pool)
        .await?;
        
        // Update memory cache
        self.update_memory_cache(did, document, handle).await;
        
        Ok(())
    }

    // Actually resolve a DID from the network
    async fn resolve_did_network(&self, did: &str) -> Result<(DidDocument, String)> {
        if did.starts_with("did:plc:") {
            self.resolve_plc_did(did).await
        } else if did.starts_with("did:web:") {
            self.resolve_web_did(did).await
        } else {
            Err(anyhow::anyhow!("Unsupported DID method: {}", did))
        }
    }

    // Resolve did:plc
    async fn resolve_plc_did(&self, did: &str) -> Result<(DidDocument, String)> {
        let url = format!("https://plc.directory/{}", did);
        let response = self.http_client.get(&url)
            .send()
            .await
            .with_context(|| "Failed to fetch PLC DID document")?;
            
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to fetch PLC DID document, status: {}", 
                response.status()
            ));
        }
        
        let document: DidDocument = response.json()
            .await
            .with_context(|| "Failed to parse PLC DID document")?;
            
        // Extract handle from alsoKnownAs
        let handle = self.extract_handle_from_document(&document)?;
        
        Ok((document, handle))
    }

    // Resolve did:web
    async fn resolve_web_did(&self, did: &str) -> Result<(DidDocument, String)> {
        // Convert did:web:example.com to https://example.com/.well-known/did.json
        let domain = did.strip_prefix("did:web:")
            .ok_or_else(|| anyhow::anyhow!("Invalid did:web format"))?;
            
        let url = format!("https://{}/.well-known/did.json", domain);
        
        let response = self.http_client.get(&url)
            .send()
            .await
            .with_context(|| "Failed to fetch Web DID document")?;
            
        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to fetch Web DID document, status: {}", 
                response.status()
            ));
        }
        
        let document: DidDocument = response.json()
            .await
            .with_context(|| "Failed to parse Web DID document")?;
            
        // Extract handle from alsoKnownAs
        let handle = self.extract_handle_from_document(&document)?;
        
        Ok((document, handle))
    }

    // Helper to extract handle from DID document
    fn extract_handle_from_document(&self, document: &DidDocument) -> Result<String> {
        if let Some(aka) = &document.also_known_as {
            for name in aka {
                // Handle formats: "at://josh.uno" or "https://bsky.app/profile/josh.uno"
                if name.starts_with("at://") {
                    return Ok(name.strip_prefix("at://").unwrap_or(name).to_string());
                }
                if name.contains("/profile/") {
                    let parts: Vec<&str> = name.split("/profile/").collect();
                    if parts.len() > 1 {
                        return Ok(parts[1].to_string());
                    }
                }
            }
        }
        
        // Fallback if no valid handle found - use truncated DID
        let fallback = did_to_fallback_handle(&document.id);
        Ok(fallback)
    }

    // Get handles for multiple DIDs in bulk
    pub async fn get_handles_bulk(&self, dids: &[String]) -> HashMap<String, String> {
        let mut result = HashMap::new();
        
        // 1. Try memory cache first for all DIDs
        {
            let cache = self.memory_cache.read().await;
            for did in dids {
                if let Some(cached) = cache.get(did) {
                    if cached.expires_at > Instant::now() {
                        result.insert(did.clone(), cached.handle.clone());
                    }
                }
            }
        }
        
        // 2. Find missing DIDs
        let missing_dids: Vec<String> = dids.iter()
            .filter(|did| !result.contains_key(*did))
            .cloned()
            .collect();
            
        if missing_dids.is_empty() {
            return result;
        }
        
        // 3. Try database cache for missing DIDs
        if let Ok(db_results) = self.get_from_db_cache_bulk(&missing_dids).await {
            for (did, doc, handle) in db_results {
                self.update_memory_cache(did.clone(), doc, handle.clone()).await;
                result.insert(did, handle);
            }
        }
        
        // 4. Find still missing DIDs
        let still_missing: Vec<String> = dids.iter()
            .filter(|did| !result.contains_key(*did))
            .cloned()
            .collect();
            
        if still_missing.is_empty() {
            return result;
        }
        
        // 5. Resolve remaining DIDs with limited concurrency
        // Use a semaphore to limit concurrent network requests
        let semaphore = Arc::new(tokio::sync::Semaphore::new(5));
        
        let resolver = Arc::new(self.clone());
        
        let resolves = still_missing.into_iter().map(|did| {
            let sem = semaphore.clone();
            let resolver = resolver.clone();
            let did_clone = did.clone();
            
            async move {
                let _permit = sem.acquire().await.unwrap();
                match resolver.resolve_did_network(&did_clone).await {
                    Ok((doc, handle)) => {
                        // Update caches asynchronously (fire and forget)
                        let resolver_clone = resolver.clone();
                        let doc_clone = doc.clone();
                        let handle_clone = handle.clone();
                        let did_clone2 = did_clone.clone(); // Clone for the closure
                        
                        tokio::spawn(async move {
                            // Clone did_clone2 again before passing to update_caches
                            let did_for_cache = did_clone2.clone();
                            let did_for_warning = did_clone2;
                            
                            if let Err(e) = resolver_clone.update_caches(did_for_cache, doc_clone, handle_clone).await {
                                warn!(did = %did_for_warning, error = %e, "Failed to update DID caches");
                            }
                        });
                        Some((did_clone, handle))
                    },
                    Err(e) => {
                        warn!(did = %did_clone, error = %e, "Failed to resolve DID");
                        let fallback = did_to_fallback_handle(&did_clone);
                        Some((did_clone, fallback))
                    }
                }
            }
        });
        
        // Wait for all resolutions to complete
        let network_results = futures::future::join_all(resolves).await;
        
        // Add network results to final map
        for item in network_results.into_iter().flatten() {
            result.insert(item.0, item.1);
        }
        
        result
    }
    
    // Fetch multiple DIDs from DB cache at once
    async fn get_from_db_cache_bulk(&self, dids: &[String]) -> Result<Vec<(String, DidDocument, String)>> {
        let mut results = Vec::new();
        
        // Using a simple loop instead of a more complex query
        // Could be optimized with an IN clause for larger sets
        for chunk in dids.chunks(50) {
            let placeholders: Vec<String> = (1..=chunk.len())
                .map(|i| format!("${}", i))
                .collect();
                
            let query = format!(
                "SELECT did, document, handle FROM did_cache 
                WHERE did IN ({}) AND expires_at > NOW()",
                placeholders.join(",")
            );
            
            let mut q = sqlx::query(&query);
            for did in chunk {
                q = q.bind(did);
            }
            
            let rows = q.fetch_all(&self.db_pool).await?;
            
            for row in rows {
                let did: String = row.get("did");
                let doc_json: serde_json::Value = row.get("document");
                let handle: String = row.get("handle");
                
                if let Ok(doc) = serde_json::from_value(doc_json) {
                    results.push((did, doc, handle));
                }
            }
        }
        
        Ok(results)
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
            "DELETE FROM did_cache WHERE expires_at <= NOW() RETURNING did"
        )
        .fetch_all(&self.db_pool)
        .await?;
        
        let db_cleaned = db_result.len();
        
        info!(
            memory_cleaned = %memory_cleaned,
            db_cleaned = %db_cleaned,
            "Cleaned expired DID cache entries"
        );
        
        Ok(memory_cleaned + db_cleaned)
    }
}

// Helper function to create a fallback handle from a DID
fn did_to_fallback_handle(did: &str) -> String {
    // Extract the last part of the DID and truncate
    let parts: Vec<&str> = did.split(':').collect();
    let last_part = parts.last().unwrap_or(&did);
    
    if last_part.len() > 8 {
        format!("user_{}", &last_part[0..8])
    } else {
        format!("user_{}", last_part)
    }
}