//! Prometheus registry of a component (spec §9, FND-03).

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

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
