use std::env;
use tracing_subscriber::{fmt, EnvFilter};

pub fn setup_logging() {
    // Check for a LOG_LEVEL environment variable, defaulting to INFO
    let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

    // Create a custom filter that limits verbose components
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Default filter configuration to reduce noise
        EnvFilter::new(format!("bluesky_push_notifier={}", log_level))
            // Set firehose and other noisy modules to WARNING level unless explicitly configured
            .add_directive("bluesky_push_notifier::firehose=warn".parse().unwrap())
            // Keep API and notification delivery at INFO level
            .add_directive("bluesky_push_notifier::api=info".parse().unwrap())
            .add_directive("bluesky_push_notifier::apns=info".parse().unwrap())
            // Reduce noise from third-party libraries
            .add_directive("tower_http=warn".parse().unwrap())
            .add_directive("a2=warn".parse().unwrap())
    });

    // Initialize the subscriber with the filter
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        // Disable unnecessary details to keep logs clean
        .with_thread_ids(false)
        .with_thread_names(false)
        .init();

    tracing::info!("Logging initialized at custom levels");
}
