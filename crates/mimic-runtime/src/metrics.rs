//! Prometheus registry of a component (spec §9, FND-03).

use prometheus_client::collector::Collector;
use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// `mimic_log_lines_dropped_total`, read from the process logger at scrape time.
#[derive(Debug)]
struct LogDrops;

impl Collector for LogDrops {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let counter = ConstCounter::new(crate::logging::dropped_lines());
        let metric = encoder.encode_descriptor(
            "log_lines_dropped",
            "Log lines dropped because stdout could not keep up",
            None,
            counter.metric_type(),
        )?;
        counter.encode(metric)
    }
}

pub struct Metrics {
    registry: Registry,
}

impl Metrics {
    /// Creates the registry with `mimic_build_info{component,version} 1`.
    pub fn new(component: &str, version: &str) -> Metrics {
        let mut registry = Registry::with_prefix("mimic");
        let build_info = Family::<Vec<(String, String)>, Gauge>::default();
        build_info
            .get_or_create(&vec![
                ("component".to_string(), component.to_string()),
                ("version".to_string(), version.to_string()),
            ])
            .set(1);
        registry.register(
            "build_info",
            "Build information of the component",
            build_info,
        );
        registry.register_collector(Box::new(LogDrops));
        Metrics { registry }
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// OpenMetrics text exposition.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing into a String cannot fail.
        let _ = encode(&mut out, &self.registry);
        out
    }
}
