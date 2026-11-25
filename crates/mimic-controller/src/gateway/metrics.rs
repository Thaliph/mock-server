//! `mimic_gateway_*` (spec §9, §7.10 "Metrics and logs"). No label holds a Gateway name, a
//! namespace, a hostname or any other `spec` value (GW-17).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::mesh::metrics::{Labels, label};

/// The values of `result`.
pub const RESULTS: [&str; 4] = ["ok", "invalid", "conflict", "error"];
/// The values of `state`.
pub const STATES: [&str; 3] = ["programmed", "pending", "not_accepted"];

#[derive(Clone, Default)]
pub struct GatewayMetrics {
    pub(crate) reconciles: Family<Labels, Counter>,
    pub(crate) gateways: Family<Labels, Gauge>,
}

impl GatewayMetrics {
    /// Registers the two families, every series at 0.
    pub fn register(registry: &mut Registry) -> GatewayMetrics {
        let metrics = GatewayMetrics::default();
        registry.register(
            "gateway_reconciles",
            "Gateway reconciles, by result (ok, invalid, conflict or error)",
            metrics.reconciles.clone(),
        );
        registry.register(
            "gateway_gateways",
            "Gateways of mimic classes at the last pass, by state (programmed, pending or not_accepted)",
            metrics.gateways.clone(),
        );
        for result in RESULTS {
            metrics.reconciles.get_or_create(&label("result", result));
        }
        for state in STATES {
            metrics.gateways.get_or_create(&label("state", state));
        }
        metrics
    }

    pub(crate) fn count(&self, result: &str) {
        self.reconciles
            .get_or_create(&label("result", result))
            .inc();
    }

    pub(crate) fn set(&self, state: &str, n: usize) {
        self.gateways
            .get_or_create(&label("state", state))
            .set(i64::try_from(n).unwrap_or(i64::MAX));
    }
}
