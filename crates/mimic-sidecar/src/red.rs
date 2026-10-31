//! RED metrics of the proxy (spec §9).

use mimic_runtime::metrics::Metrics;

/// The proxy's request, connection and byte counters.
#[derive(Clone, Default)]
pub struct ProxyMetrics {}

impl ProxyMetrics {
    pub fn register(_metrics: &mut Metrics, _max_authorities: usize) -> ProxyMetrics {
        ProxyMetrics {}
    }
}
