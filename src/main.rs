mod config;
mod db;
mod filter;
mod firehose;
mod logging;
mod metrics;
mod models;
mod stream;
mod subscription;

use anyhow::Result;
use tokio::{
    signal,
    sync::{mpsc, oneshot},
};
use tracing::{error, info};

fn main() -> Result<()> {
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
        logging::setup_logging();
        dotenv::dotenv().ok();

        info!("Starting Bluesky Push Notification Service (firehose consumer)");
        info!("Events will be enqueued to push_event_queue for nest to deliver");

        let config = config::Config::from_env()?;
        let db_pool = db::init_db_pool(&config.database_url).await?;

        // Ensure firehose_cursor table exists (other tables managed by nest)
        db::ensure_firehose_cursor_table(&db_pool).await?;

        // One-time cleanup of old cursors
        if let Err(e) = db::cleanup_old_cursors(&db_pool, 1).await {
            error!("Error during cursor cleanup: {}", e);
        }

        // Hourly cursor cleanup
        let db_pool_cleanup = db_pool.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if let Err(e) = db::cleanup_old_cursors(&db_pool_cleanup, 1).await {
                    error!("Error cleaning up cursor history: {}", e);
                }
            }
        });

        // Create channel for firehose events
        let (event_sender, event_receiver) = mpsc::channel(1000);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Spawn firehose consumer
        let mut firehose_handle = tokio::spawn(firehose::run_firehose_consumer(
            config.bsky_service_url.clone(),
            event_sender,
            db_pool.clone(),
            shutdown_rx,
        ));

        // Spawn event filter/enqueuer (writes to push_event_queue)
        let mut filter_handle = tokio::spawn(filter::run_event_filter(
            event_receiver,
            db_pool.clone(),
        ));

        // Minimal health/metrics server
        let health_handle = tokio::spawn(async move {
            let addr = std::env::var("API_BIND_ADDRESS")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string());
            info!("Starting health server on {}", addr);

            let app = axum::Router::new()
                .route("/health", axum::routing::get(|| async { "OK" }))
                .route(
                    "/metrics",
                    axum::routing::get(|| async { metrics::metrics_handler() }),
                );

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to bind health server: {}", e);
                    return;
                }
            };
            if let Err(e) = axum::serve(listener, app).await {
                error!("Health server error: {}", e);
            }
        });

        // Monitor for shutdown or task failure
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal");
            }
            result = &mut firehose_handle => {
                match result {
                    Ok(Ok(())) => info!("Firehose task exited cleanly"),
                    Ok(Err(e)) => error!("Firehose task failed: {}", e),
                    Err(e) => error!("Firehose task panicked: {}", e),
                }
                error!("Critical: Firehose task stopped unexpectedly");
            }
            result = &mut filter_handle => {
                match result {
                    Ok(Ok(())) => info!("Filter task exited cleanly"),
                    Ok(Err(e)) => error!("Filter task failed: {}", e),
                    Err(e) => error!("Filter task panicked: {}", e),
                }
                error!("Critical: Filter task stopped unexpectedly");
            }
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::join!(firehose_handle, filter_handle, health_handle);

        info!("Shutdown complete");
        Ok(())
    })
}
