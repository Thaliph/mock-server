//! MSH-31 (configuration, retry delay) and MSH-32 (the table).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use mimic_core::mesh::view::{Limits, View};
use mimic_runtime::config::Env;

use super::*;

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn config(pairs: &[(&str, &str)]) -> Result<MeshConfig, String> {
    MeshConfig::from_env(&env(pairs), std::path::Path::new("/mimic")).map_err(|e| e.to_string())
}

#[test]
fn msh_31_defaults() {
    let config = config(&[]).unwrap();
    assert!(!config.enabled);
    assert_eq!(config.stale_after, Duration::from_secs(60));
    assert_eq!(config.startup_wait, Duration::from_secs(2));
    assert_eq!(config.limits, Limits::default());
    assert!(config.target.is_none());
    assert_eq!(
        config.state_file,
        std::path::PathBuf::from("/mimic/state/mesh.json")
    );
}

#[test]
fn msh_31_values_are_read() {
    let config = config(&[
        ("MIMIC_MESH", "on"),
        ("MIMIC_MESH_STALE_AFTER", "1s"),
        ("MIMIC_MESH_STARTUP_WAIT", "0s"),
        ("MIMIC_MESH_MAX_SERVICES", "100000"),
        ("MIMIC_MESH_MAX_ENDPOINTS", "1000000"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "256Mi"),
    ])
    .unwrap();
    assert!(config.enabled);
    assert_eq!(config.stale_after, Duration::from_secs(1));
    assert_eq!(config.startup_wait, Duration::ZERO);
    assert_eq!(config.limits.max_services, 100_000);
    assert_eq!(config.limits.max_endpoints, 1_000_000);
    assert_eq!(config.limits.max_view_bytes, 256 << 20);
}

#[test]
fn msh_31_invalid_values_are_refused() {
    for (var, value) in [
        ("MIMIC_MESH", "yes"),
        ("MIMIC_MESH_STALE_AFTER", "999ms"),
        ("MIMIC_MESH_STALE_AFTER", "3601s"),
        ("MIMIC_MESH_STALE_AFTER", "soon"),
        ("MIMIC_MESH_STARTUP_WAIT", "61s"),
        ("MIMIC_MESH_MAX_SERVICES", "0"),
        ("MIMIC_MESH_MAX_SERVICES", "100001"),
        ("MIMIC_MESH_MAX_ENDPOINTS", "0"),
        ("MIMIC_MESH_MAX_ENDPOINTS", "1000001"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "1Ki"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "512Mi"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "4M"),
    ] {
        let error = config(&[(var, value)]).expect_err(&format!("{var}={value}"));
        assert!(error.contains(var), "{error}");
    }
}

#[test]
fn msh_31_the_retry_delay() {
    let secs = |failures, factor| retry_delay(failures, factor).as_secs_f64();
    // 1, 2, 4 ... 30 s at the top of the range, half of it at the bottom, never below 1 s.
    assert_eq!(secs(0, 1.0), 1.0);
    assert_eq!(secs(0, 0.5), 1.0);
    assert_eq!(secs(1, 1.0), 2.0);
    assert_eq!(secs(2, 1.0), 4.0);
    assert_eq!(secs(3, 0.5), 4.0);
    assert_eq!(secs(4, 1.0), 16.0);
    assert_eq!(secs(5, 1.0), 30.0);
    assert_eq!(secs(5, 0.5), 15.0);
    assert_eq!(secs(u32::MAX, 1.0), 30.0);
    for failures in 0..40 {
        for factor in [0.5, 0.75, 1.0] {
            let d = secs(failures, factor);
            assert!((1.0..=30.0).contains(&d), "{failures} {factor} {d}");
        }
    }
}

fn view(text: &str) -> View {
    View::from_json(text, &Limits::default()).unwrap()
}

fn service(key: &str, ips: &[&str], ports: &[u16]) -> String {
    let ips: Vec<String> = ips.iter().map(|ip| format!("\"{ip}\"")).collect();
    let ports: Vec<String> = ports
        .iter()
        .map(|p| format!("\"{p}\":{{\"targetPort\":8080}}"))
        .collect();
    format!(
        "\"{key}\":{{\"endpoints\":{{\"80\":[{{\"ip\":\"10.1.0.9\",\"port\":8080,\"ready\":true}}]}},\"generations\":{{}},\"ips\":[{}],\"policy\":{{\"loadBalancer\":{{\"slowStartSeconds\":30}}}},\"ports\":{{{}}}}}",
        ips.join(","),
        ports.join(",")
    )
}

fn doc(mesh: bool, services: &[String]) -> String {
    format!(
        "{{\"cluster\":\"local\",\"format\":1,\"generations\":{{}},\"mesh\":{mesh},\"mode\":\"none\",\"services\":{{{}}}}}",
        services.join(",")
    )
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[test]
fn msh_32_the_table_maps_cluster_ip_and_port_to_the_service() {
    let table = Table::build(&view(&doc(
        true,
        &[
            service("shop/cart", &["10.96.0.10", "fd00::10"], &[80, 8443]),
            service("shop/headless", &[], &[80]),
            service("other/pay", &["10.96.0.11"], &[9000]),
        ],
    )));
    assert_eq!(table.lookup(ip("10.96.0.10"), 80), Some("shop/cart"));
    assert_eq!(table.lookup(ip("10.96.0.10"), 8443), Some("shop/cart"));
    assert_eq!(table.lookup(ip("fd00::10"), 80), Some("shop/cart"));
    assert_eq!(table.lookup(ip("10.96.0.11"), 9000), Some("other/pay"));
    // Another port, a pod IP, an unknown address.
    assert_eq!(table.lookup(ip("10.96.0.10"), 81), None);
    assert_eq!(table.lookup(ip("10.1.0.9"), 8080), None);
    assert_eq!(table.lookup(ip("10.1.0.9"), 80), None);
    assert_eq!(table.lookup(ip("192.0.2.1"), 80), None);
    // Five entries (two addresses and two ports of cart, one of pay): none for the headless Service.
    assert_eq!(table.len(), 5);
}

#[test]
fn msh_32_classification() {
    let state = MeshState::default();
    let dst = |s: &str| s.parse::<SocketAddr>().unwrap();
    // Before the first view every destination is unscoped.
    assert_eq!(state.scope(dst("10.96.0.10:80")), Scope::Unscoped);
    state.hold(
        view(&doc(true, &[service("shop/cart", &["10.96.0.10"], &[80])])),
        "\"1-a\"",
    );
    assert_eq!(state.scope(dst("10.96.0.10:80")), Scope::InScope);
    // An IPv4-mapped address is the IPv4 one.
    assert_eq!(state.scope(dst("[::ffff:10.96.0.10]:80")), Scope::InScope);
    assert_eq!(state.scope(dst("10.96.0.10:81")), Scope::Unscoped);
    assert_eq!(state.scope(dst("10.1.0.9:8080")), Scope::Unscoped);
    // The mesh off for this workload: nothing is classified.
    state.hold(view(&doc(false, &[])), "\"1-b\"");
    assert_eq!(state.scope(dst("10.96.0.10:80")), Scope::Off);
    assert_eq!(state.scope(dst("192.0.2.1:80")), Scope::Off);
}

#[test]
fn msh_32_only_unscoped_connections_are_counted() {
    let state = MeshState::default();
    state.hold(
        view(&doc(true, &[service("shop/cart", &["10.96.0.10"], &[80])])),
        "\"1-a\"",
    );
    state.classify("10.96.0.10:80".parse().unwrap());
    assert_eq!(state.unscoped_connections(), 0);
    state.classify("192.0.2.1:80".parse().unwrap());
    state.classify("192.0.2.1:80".parse().unwrap());
    assert_eq!(state.unscoped_connections(), 2);
    state.hold(view(&doc(false, &[])), "\"1-b\"");
    state.classify("192.0.2.1:80".parse().unwrap());
    assert_eq!(state.unscoped_connections(), 2);
}

#[test]
fn msh_32_a_swapped_table_never_mixes_with_the_old_one() {
    let state = MeshState::default();
    state.hold(
        view(&doc(true, &[service("shop/cart", &["10.96.0.10"], &[80])])),
        "\"1-a\"",
    );
    let before = state.table();
    state.hold(
        view(&doc(true, &[service("shop/pay", &["10.96.0.11"], &[80])])),
        "\"1-b\"",
    );
    // A holder of the old table sees all of it, and only it.
    assert_eq!(before.lookup(ip("10.96.0.10"), 80), Some("shop/cart"));
    assert_eq!(before.lookup(ip("10.96.0.11"), 80), None);
    let after = state.table();
    assert_eq!(after.lookup(ip("10.96.0.10"), 80), None);
    assert_eq!(after.lookup(ip("10.96.0.11"), 80), Some("shop/pay"));
}

#[test]
fn msh_47_pool_and_endpoint_label_defaults_and_values() {
    let defaults = config(&[]).unwrap();
    assert_eq!(defaults.pool_idle_timeout, Duration::from_secs(90));
    assert_eq!(defaults.max_endpoint_labels, 256);
    let set = config(&[
        ("MIMIC_MESH_POOL_IDLE_TIMEOUT", "1s"),
        ("MIMIC_METRICS_MAX_ENDPOINTS", "10000"),
    ])
    .unwrap();
    assert_eq!(set.pool_idle_timeout, Duration::from_secs(1));
    assert_eq!(set.max_endpoint_labels, 10_000);
    assert_eq!(
        config(&[("MIMIC_MESH_POOL_IDLE_TIMEOUT", "1h")])
            .unwrap()
            .pool_idle_timeout,
        Duration::from_secs(3600)
    );
}

#[test]
fn msh_47_invalid_pool_and_endpoint_label_values_are_refused() {
    for (var, value) in [
        ("MIMIC_MESH_POOL_IDLE_TIMEOUT", "999ms"),
        ("MIMIC_MESH_POOL_IDLE_TIMEOUT", "3601s"),
        ("MIMIC_MESH_POOL_IDLE_TIMEOUT", "soon"),
        ("MIMIC_METRICS_MAX_ENDPOINTS", "0"),
        ("MIMIC_METRICS_MAX_ENDPOINTS", "10001"),
        ("MIMIC_METRICS_MAX_ENDPOINTS", "many"),
    ] {
        let error = config(&[(var, value)]).expect_err(&format!("{var}={value}"));
        assert!(error.contains(var), "{error}");
    }
}
