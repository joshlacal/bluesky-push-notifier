use std::env;
use tracing_subscriber::{fmt, EnvFilter};

fn build_default_filter() -> EnvFilter {
    let base_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

    let mut filter = EnvFilter::new(format!("bluesky_push_notifier={base_level}"));

    for directive in [
        "bluesky_push_notifier::firehose=info",
        "bluesky_push_notifier::filter=warn",
        "bluesky_push_notifier::stream=warn",
        "bluesky_push_notifier::subscription=warn",
        "sqlx=warn",
    ] {
        filter = filter.add_directive(directive.parse().expect("invalid log directive"));
    }

    filter
}

pub fn setup_logging() {
    let mut filter = build_default_filter();

    if let Ok(extra) = env::var("EXTRA_LOG_DIRECTIVES") {
        for directive in extra.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            if let Ok(parsed) = directive.parse() {
                filter = filter.add_directive(parsed);
            }
        }
    }

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .init();

    tracing::info!("Logging initialized");
}
