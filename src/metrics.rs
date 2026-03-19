use lazy_static::lazy_static;
use prometheus::{register_counter, register_histogram, Counter, Histogram, HistogramOpts, Opts};

lazy_static! {
    pub static ref EVENTS_PROCESSED: Counter = register_counter!(Opts::new(
        "events_processed_total",
        "Total number of firehose events processed"
    ))
    .unwrap();

    pub static ref EVENTS_ENQUEUED: Counter = register_counter!(Opts::new(
        "events_enqueued_total",
        "Total number of events enqueued to push_event_queue"
    ))
    .unwrap();

    pub static ref EVENT_PROCESSING_TIME: Histogram = register_histogram!(
        HistogramOpts::new(
            "event_processing_time_seconds",
            "Time taken to process a firehose event"
        )
        .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0])
    )
    .unwrap();
}

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
