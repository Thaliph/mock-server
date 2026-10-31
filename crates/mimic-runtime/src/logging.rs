//! JSON logs on stdout, one object per event (spec §9, FND-04).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::ParseError;

pub fn filter(spec: &str) -> Result<EnvFilter, ParseError> {
    EnvFilter::try_new(spec)
}

/// Installs the global subscriber. `spec` was validated by `CommonConfig::from_env`.
pub fn init(spec: &str) {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_env_filter(filter(spec).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stdout)
        .init();
}
