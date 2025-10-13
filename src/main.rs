mod activity_subscription_manager;
mod api;
mod apns;
mod app_attest;
mod config;
mod crypto; // Add the new crypto module
mod db;
mod did_resolver;
mod filter;
mod firehose;
mod logging;
mod metrics;
mod models;
mod post_resolver;
mod relationship_manager;
mod stream;
mod subscription;

use activity_subscription_manager::ActivitySubscriptionManager;
use anyhow::Result;
use app_attest::AppAttestService;
use relationship_manager::RelationshipManager;
use std::sync::Arc;
use tokio::{
    signal,
    sync::{mpsc, oneshot},
};
use tracing::error;
use tracing::info;

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

        let activity_subscription_manager =
            Arc::new(ActivitySubscriptionManager::new(db_pool.clone()));

        let app_attest_service = Arc::new(AppAttestService::new(
            config.app_attest_app_id.clone(),
            config.app_attest_challenge_ttl_secs,
            config.app_attest_production,
        ));

        // One-time cleanup to fix existing cursor issue
        info!("Running one-time cleanup of firehose cursor table");
        if let Err(e) = db::cleanup_old_cursors(&db_pool, 1).await {
            error!("Error during one-time cursor cleanup: {}", e);
        }

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

        // Spawn cursor cleanup task
        let db_pool_clone = db_pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // hourly
            loop {
                interval.tick().await;
                // Keep only 1 day of history
                if let Err(e) = db::cleanup_old_cursors(&db_pool_clone, 1).await {
                    tracing::error!("Error cleaning up cursor history: {}", e);
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
        // Ensure we pass only host to PostResolver (it prefixes https:// itself)
        let mut bsky_api_host = config.bsky_api_url.clone();
        if let Some(stripped) = bsky_api_host
            .strip_prefix("https://")
            .or_else(|| bsky_api_host.strip_prefix("http://"))
        {
            bsky_api_host = stripped.to_string();
        }
        bsky_api_host = bsky_api_host.trim_end_matches('/').to_string();

        let post_resolver = Arc::new(post_resolver::PostResolver::new(
            db_pool.clone(),
            60, // 60 minute TTL
            bsky_api_host,
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
            &config.apns_topic,
        )?;

        // Create channels for notification pipeline
        let (event_sender, event_receiver) = mpsc::channel(1000);
        let (notification_sender, notification_receiver) = mpsc::channel(1000);

        // Create shutdown signal
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Spawn firehose consumer task
        let mut firehose_handle = tokio::spawn(firehose::run_firehose_consumer(
            config.bsky_service_url.clone(),
            event_sender,
            db_pool.clone(),
            shutdown_rx,
        ));

        let mut filter_handle = tokio::spawn(filter::run_event_filter(
            event_receiver,
            notification_sender,
            db_pool.clone(),
            did_resolver.clone(),
            post_resolver.clone(),
            relationship_manager.clone(), // Add relationship manager
            activity_subscription_manager.clone(),
        ));

        // Spawn notification sender task
        let mut apns_handle = tokio::spawn(apns::run_notification_sender(
            notification_receiver,
            apns_client,
            db_pool.clone(),
        ));

        // Spawn API server
        let db_pool_clone = db_pool.clone();
        let api_state = Arc::new(api::ApiState {
            db_pool: db_pool_clone,
            relationship_manager: relationship_manager.clone(), // Add relationship manager
            app_attest: app_attest_service,
            activity_subscription_manager: activity_subscription_manager.clone(),
        });
        let api_router = api::create_api_router(api_state);

        let api_handle = tokio::spawn(async move {
            let addr =
                std::env::var("API_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

            info!("Starting API server on {}", addr);

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    error!("Failed to bind API server to {}: {}", addr, e);
                    return;
                }
            };

            if let Err(e) = axum::serve(listener, api_router).await {
                error!("API server error: {}", e);
            }
        });

        // Handle graceful shutdown and monitor for task failures
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal, shutting down gracefully");
            }
            result = &mut firehose_handle => {
                match result {
                    Ok(Ok(())) => info!("Firehose task exited cleanly"),
                    Ok(Err(e)) => error!("Firehose task failed: {}", e),
                    Err(e) => error!("Firehose task panicked: {}", e),
                }
                error!("Critical: Firehose task stopped unexpectedly, initiating shutdown");
            }
            result = &mut filter_handle => {
                match result {
                    Ok(Ok(())) => info!("Filter task exited cleanly"),
                    Ok(Err(e)) => error!("Filter task failed: {}", e),
                    Err(e) => error!("Filter task panicked: {}", e),
                }
                error!("Critical: Filter task stopped unexpectedly, initiating shutdown");
            }
            result = &mut apns_handle => {
                match result {
                    Ok(Ok(())) => info!("APNS task exited cleanly"),
                    Ok(Err(e)) => error!("APNS task failed: {}", e),
                    Err(e) => error!("APNS task panicked: {}", e),
                }
                error!("Critical: APNS task stopped unexpectedly, initiating shutdown");
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
