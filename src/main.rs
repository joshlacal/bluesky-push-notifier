mod api;
mod apns;
mod config;
mod db;
mod filter;
mod firehose;
mod logging;
mod models;
mod stream;
mod subscription;
mod did_resolver;
mod post_resolver;
mod metrics;
mod relationship_manager;

use anyhow::Result;
use std::sync::Arc;
use tokio::{
    signal,
    sync::{mpsc, oneshot},
};
use tracing::info;
use relationship_manager::RelationshipManager;

fn main() -> Result<()> {
    // Build custom runtime with explicit thread configuration
    let worker_threads = std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(num_cpus::get);
        
    println!("Starting with {} Tokio worker threads", worker_threads);
    
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .unwrap();
    
    runtime.block_on(async {
        // Initialize logging first thing
        logging::setup_logging();

        // Load environment variables from .env file if present
        dotenv::dotenv().ok();

        info!("Starting Bluesky Push Notification Service");

        // Load configuration
        let config = config::Config::from_env()?;

        // Initialize database connection pool
        let db_pool = db::init_db_pool(&config.database_url).await?;

        // Initialize relationship manager with moka cache
        let relationship_manager = Arc::new(RelationshipManager::new(db_pool.clone()));

        // Start background task for relationship cache maintenance
        let relationship_manager_clone = relationship_manager.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // hourly
            loop {
                interval.tick().await;
                if let Err(e) = relationship_manager_clone.run_cache_maintenance().await {
                    tracing::error!("Error during relationship cache maintenance: {}", e);
                }
            }
        });

        let did_resolver = Arc::new(did_resolver::DidResolver::new(db_pool.clone(), 24));

        let did_resolver_clone = did_resolver.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // hourly
            loop {
                interval.tick().await;
                if let Err(e) = did_resolver_clone.cleanup_expired().await {
                    tracing::error!("Error cleaning up DID cache: {}", e);
                }
            }
        });

        // After initializing did_resolver
        let post_resolver = Arc::new(post_resolver::PostResolver::new(
            db_pool.clone(),
            60, // 60 minute TTL
            std::env::var("BSKY_API_URL").unwrap_or_else(|_| "https://public.api.bsky.app".to_string())
        ));

        // Start post_resolver cleanup task
        let post_resolver_clone = post_resolver.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // hourly
            loop {
                interval.tick().await;
                if let Err(e) = post_resolver_clone.cleanup_expired().await {
                    tracing::error!("Error cleaning up post cache: {}", e);
                }
            }
        });

        // Initialize APNs client
        let apns_client = apns::ApnsClient::new(
            &config.apns_key_path,
            &config.apns_key_id,
            &config.apns_team_id,
            config.apns_production,
        )?;

        // Create channels for notification pipeline
        let (event_sender, event_receiver) = mpsc::channel(1000);
        let (notification_sender, notification_receiver) = mpsc::channel(1000);

        // Create shutdown signal
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Spawn firehose consumer task
        let firehose_handle = tokio::spawn(firehose::run_firehose_consumer(
            config.bsky_service_url.clone(),
            event_sender,
            db_pool.clone(),
            shutdown_rx,
        ));

        let filter_handle = tokio::spawn(filter::run_event_filter(
            event_receiver,
            notification_sender,
            db_pool.clone(),
            did_resolver.clone(),
            post_resolver.clone(),
            relationship_manager.clone(), // Add relationship manager
        ));

        // Spawn notification sender task
        let apns_handle = tokio::spawn(apns::run_notification_sender(
            notification_receiver,
            apns_client,
            db_pool.clone(),
        ));

        // Spawn API server
        let db_pool_clone = db_pool.clone();
        let api_state = Arc::new(api::ApiState {
            db_pool: db_pool_clone,
            relationship_manager: relationship_manager.clone(), // Add relationship manager
        });
        let api_router = api::create_api_router(api_state);

        let api_handle = tokio::spawn(async move {
            let addr = std::env::var("API_BIND_ADDRESS")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string());
                
            info!("Starting API server on {}", addr);
            
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, api_router).await.unwrap();
        });

        // Handle graceful shutdown
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal, shutting down gracefully");
            }
        }

        // Send shutdown signal to tasks
        let _ = shutdown_tx.send(());

        // Wait for ALL tasks to complete, including api_handle
        let _ = tokio::join!(firehose_handle, filter_handle, apns_handle, api_handle);

        info!("Shutdown complete");
        Ok(())
    })
}
