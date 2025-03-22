//metrics.rs
use lazy_static::lazy_static;
use prometheus::{register_counter, register_histogram, Counter, Histogram, HistogramOpts, Opts};

// Define metrics
lazy_static! {
    // Event metrics
    pub static ref EVENTS_PROCESSED: Counter = register_counter!(Opts::new(
        "events_processed_total",
        "Total number of events processed"
    ))
    .unwrap();
    
    pub static ref NOTIFICATIONS_SENT: Counter = register_counter!(Opts::new(
        "notifications_sent_total",
        "Total number of notifications sent"
    ))
    .unwrap();
    
    // Cache metrics
    pub static ref DID_CACHE_HITS: Counter = register_counter!(Opts::new(
        "did_cache_hits_total",
        "Total number of DID cache hits"
    ))
    .unwrap();
    
    pub static ref DID_CACHE_MISSES: Counter = register_counter!(Opts::new(
        "did_cache_misses_total",
        "Total number of DID cache misses"
    ))
    .unwrap();
    
    pub static ref POST_CACHE_HITS: Counter = register_counter!(Opts::new(
        "post_cache_hits_total",
        "Total number of post cache hits"
    ))
    .unwrap();
    
    pub static ref POST_CACHE_MISSES: Counter = register_counter!(Opts::new(
        "post_cache_misses_total",
        "Total number of post cache misses"
    ))
    .unwrap();
    
    // Timing metrics
    pub static ref EVENT_PROCESSING_TIME: Histogram = register_histogram!(
        HistogramOpts::new(
            "event_processing_time_seconds",
            "Time taken to process an event"
        )
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0])
    )
    .unwrap();
    
    pub static ref DID_RESOLUTION_TIME: Histogram = register_histogram!(
        HistogramOpts::new(
            "did_resolution_time_seconds",
            "Time taken to resolve a DID"
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
    )
    .unwrap();
    
    pub static ref POST_FETCH_TIME: Histogram = register_histogram!(
        HistogramOpts::new(
            "post_fetch_time_seconds",
            "Time taken to fetch a post"
        )
        .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0])
    )
    .unwrap();

    // Add batch-specific metrics
    pub static ref POST_BATCH_SIZE: Histogram = register_histogram!(
        HistogramOpts::new(
            "post_batch_size",
            "Size of batched post requests"
        )
        .buckets(vec![1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 25.0])
    )
    .unwrap();

    pub static ref POST_BATCH_LATENCY: Histogram = register_histogram!(
        HistogramOpts::new(
            "post_batch_latency_seconds",
            "Latency of batched post requests"
        )
        .buckets(vec![0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.2, 0.3, 0.5])
    )
    .unwrap();
}

// Function to expose metrics endpoint
pub fn metrics_handler() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buffer = Vec::new();
    
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
        return format!("Error encoding metrics: {}", e);
    }
    
    match String::from_utf8(buffer) {
        Ok(metrics) => metrics,
        Err(e) => format!("Error converting metrics to string: {}", e),
    }
}