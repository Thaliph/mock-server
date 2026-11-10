//! `mimic_limit{name}` (spec §9, #385): the configured limits of the sidecar, set at start and
//! constant for the process. The names are fixed; no value is a URL, a path or a secret.

use std::collections::BTreeMap;

use mimic_runtime::metrics::Metrics;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;

use crate::discovery::DiscoveryConfig;
use crate::egress::EgressConfig;
use crate::listener::TunnelConfig;
use crate::mode::ModeConfig;

/// The limits of a parsed configuration, by `name` (sorted). Sizes are in bytes.
pub fn limits(
    tunnel: &TunnelConfig,
    discovery: &DiscoveryConfig,
    egress: &EgressConfig,
    mode: &ModeConfig,
) -> BTreeMap<&'static str, u64> {
    let n = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    BTreeMap::from([
        ("max_connections", n(tunnel.max_connections)),
        ("metrics_max_authorities", n(tunnel.max_authorities)),
        ("discovery_max_endpoints", n(discovery.max_endpoints)),
        ("discovery_max_authorities", n(discovery.max_authorities)),
        ("discovery_sample_budget_bytes", n(discovery.sample_budget)),
        (
            "discovery_capture_budget_bytes",
            n(discovery.capture_budget),
        ),
        ("discovery_queue", n(discovery.queue)),
        ("egress_max_bytes", n(egress.max_bytes)),
        ("mock_max_pending", n(mode.max_pending)),
    ])
}

/// Registers `mimic_limit` with one series per entry of `limits`.
pub fn register(metrics: &mut Metrics, limits: &BTreeMap<&'static str, u64>) {
    let family = Family::<Vec<(String, String)>, Gauge>::default();
    for (name, value) in limits {
        family
            .get_or_create(&vec![("name".to_string(), (*name).to_string())])
            .set(i64::try_from(*value).unwrap_or(i64::MAX));
    }
    metrics
        .registry_mut()
        .register("limit", "Configured limit, by name", family);
}
