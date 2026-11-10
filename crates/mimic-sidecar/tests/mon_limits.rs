//! MON-06 (sidecar part): the `mimic_limit` series (spec §9, #385).

use std::collections::BTreeMap;

use mimic_runtime::config::Env;
use mimic_runtime::testing::{Process, http_get};
use mimic_sidecar::discovery::DiscoveryConfig;
use mimic_sidecar::egress::EgressConfig;
use mimic_sidecar::limits::limits;
use mimic_sidecar::listener::TunnelConfig;
use mimic_sidecar::mode::ModeConfig;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

const NAMES: [&str; 9] = [
    "discovery_capture_budget_bytes",
    "discovery_max_authorities",
    "discovery_max_endpoints",
    "discovery_queue",
    "discovery_sample_budget_bytes",
    "egress_max_bytes",
    "max_connections",
    "metrics_max_authorities",
    "mock_max_pending",
];

fn parsed(pairs: &[(&str, &str)]) -> BTreeMap<&'static str, u64> {
    let env = Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    limits(
        &TunnelConfig::from_env(&env).unwrap(),
        &DiscoveryConfig::from_env(&env).unwrap(),
        &EgressConfig::from_env(&env).unwrap(),
        &ModeConfig::from_env(&env, std::path::Path::new("/mimic")).unwrap(),
    )
}

#[test]
fn mon_06_the_sidecar_limits_are_exactly_the_documented_names_with_the_parsed_values() {
    let set = parsed(&[
        ("MIMIC_MAX_CONNECTIONS", "100"),
        ("MIMIC_EGRESS_MAX_BYTES", "16Mi"),
        ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "32Mi"),
        ("MIMIC_DISCOVERY_CAPTURE_BUDGET", "1Mi"),
        ("MIMIC_DISCOVERY_QUEUE", "7"),
        ("MIMIC_DISCOVERY_MAX_ENDPOINTS", "11"),
        ("MIMIC_DISCOVERY_MAX_AUTHORITIES", "12"),
        ("MIMIC_METRICS_MAX_AUTHORITIES", "13"),
        ("MIMIC_MOCK_MAX_PENDING", "14"),
    ]);
    assert_eq!(set.keys().copied().collect::<Vec<_>>(), NAMES);
    assert_eq!(set["max_connections"], 100);
    assert_eq!(set["egress_max_bytes"], 16_777_216);
    assert_eq!(set["discovery_sample_budget_bytes"], 33_554_432);
    assert_eq!(set["discovery_capture_budget_bytes"], 1_048_576);
    assert_eq!(set["discovery_queue"], 7);
    assert_eq!(set["discovery_max_endpoints"], 11);
    assert_eq!(set["discovery_max_authorities"], 12);
    assert_eq!(set["metrics_max_authorities"], 13);
    assert_eq!(set["mock_max_pending"], 14);
}

#[test]
fn mon_06_the_defaults_of_the_configuration_apply_when_unset_and_equal_inputs_give_equal_outputs() {
    let set = parsed(&[]);
    assert_eq!(set.keys().copied().collect::<Vec<_>>(), NAMES);
    assert_eq!(set["max_connections"], 16_384);
    assert_eq!(set["egress_max_bytes"], 4 * 1024 * 1024);
    assert_eq!(set["discovery_sample_budget_bytes"], 16 * 1024 * 1024);
    assert_eq!(set["discovery_capture_budget_bytes"], 8 * 1024 * 1024);
    assert_eq!(set["discovery_queue"], 1024);
    assert_eq!(set["discovery_max_endpoints"], 256);
    assert_eq!(set["discovery_max_authorities"], 64);
    assert_eq!(set["metrics_max_authorities"], 50);
    assert_eq!(set["mock_max_pending"], 1024);
    assert_eq!(set, parsed(&[]));
}

fn limit_lines(admin: std::net::SocketAddr) -> Vec<String> {
    let (status, text) = http_get(admin, "/metrics");
    assert_eq!(status, 200);
    text.lines()
        .filter(|l| l.starts_with("mimic_limit{"))
        .map(str::to_string)
        .collect()
}

#[test]
fn mon_06_the_sidecar_metrics_hold_the_limits_unchanged_across_two_scrapes() {
    let mut process = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_MAX_CONNECTIONS", "100"),
            ("MIMIC_EGRESS_MAX_BYTES", "16Mi"),
        ],
    );
    let admin = process.admin_addr();
    let first = limit_lines(admin);
    assert_eq!(first.len(), NAMES.len(), "{first:?}");
    for name in NAMES {
        assert!(
            first
                .iter()
                .any(|l| l.starts_with(&format!("mimic_limit{{name=\"{name}\"}} "))),
            "{name} in {first:?}"
        );
    }
    assert!(first.contains(&"mimic_limit{name=\"max_connections\"} 100".to_string()));
    assert!(first.contains(&"mimic_limit{name=\"egress_max_bytes\"} 16777216".to_string()));
    assert_eq!(first, limit_lines(admin));
}
