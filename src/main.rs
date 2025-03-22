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

use anyhow::Result;
use std::sync::Arc;
use tokio::{
    signal,
    sync::{mpsc, oneshot},
};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging first thing
    logging::setup_logging();

    // Load environment variables from .env file if present
    dotenv::dotenv().ok();

    info!("Starting Bluesky Push Notification Service");

    // Load configuration
    let config = config::Config::from_env()?;

    // Initialize database connection pool
    let db_pool = db::init_db_pool(&config.database_url).await?;

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

    // Spawn event filter task - Pass did_resolver as the 4th argument
    let filter_handle = tokio::spawn(filter::run_event_filter(
        event_receiver,
        notification_sender,
        db_pool.clone(),
        did_resolver.clone(),
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
    });
    let api_router = api::create_api_router(api_state);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
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
}
