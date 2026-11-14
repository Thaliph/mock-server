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
        "\"{key}\":{{\"endpoints\":{{\"80\":[{{\"ip\":\"10.1.0.9\",\"port\":8080,\"ready\":true}}]}},\"generations\":{{}},\"ips\":[{}],\"policy\":{{\"loadBalancer\":{{\"slowStartSeconds\":30}},\"retries\":3,\"retryBudget\":{{\"minPerSecond\":10,\"percent\":20,\"windowSeconds\":10}}}},\"ports\":{{{}}}}}",
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

#[test]
fn msh_53_the_first_head_says_whether_the_connection_stays_on_the_byte_relay() {
    use super::{Head, head_kind};
    assert_eq!(head_kind(b""), Head::Partial);
    assert_eq!(head_kind(b"GET / HTTP/1.1\r\nHost: a\r\n"), Head::Partial);
    assert_eq!(
        head_kind(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"),
        Head::Routed
    );
    assert_eq!(head_kind(b"GET / HTTP/1.1\nHost: a\n\n"), Head::Routed);
    assert_eq!(
        head_kind(b"POST /u HTTP/1.1\r\nHost: a\r\nContent-Length: 2\r\n\r\nab"),
        Head::Routed
    );
    for relay in [
        &b"GET /ws HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"[..],
        b"GET /ws HTTP/1.1\r\nHost: a\r\nupgrade: h2c\r\n\r\n",
        b"GET /ws HTTP/1.1\r\nHost: a\r\nUPGRADE : h2c\r\n\r\n",
        b"GET /ws HTTP/1.1\nHost: a\nUpgrade: websocket\n\n",
        b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
        b"connect example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
    ] {
        assert_eq!(
            head_kind(relay),
            Head::Relay,
            "{}",
            String::from_utf8_lossy(relay)
        );
    }
    // A value that mentions upgrade is not the field.
    assert_eq!(
        head_kind(b"GET / HTTP/1.1\r\nHost: a\r\nX-Note: Upgrade: now\r\n\r\n"),
        Head::Routed
    );
}

#[test]
fn msh_57_opaque_ports_default_and_override() {
    let ports = |c: MeshConfig| c.opaque_ports.into_iter().collect::<Vec<u16>>();
    assert_eq!(
        ports(config(&[]).unwrap()),
        vec![21, 22, 25, 587, 3306, 4444, 5432, 6379, 9300, 11211]
    );
    assert_eq!(
        ports(config(&[("MIMIC_MESH_OPAQUE_PORTS", " 8080 , 9 ,8080")]).unwrap()),
        vec![9, 8080]
    );
    assert!(ports(config(&[("MIMIC_MESH_OPAQUE_PORTS", "none")]).unwrap()).is_empty());
}

#[test]
fn msh_57_invalid_opaque_ports_are_refused() {
    let many = (1..=257)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    for value in ["0", "65536", "http", "80,,81", "80,", "-1", many.as_str()] {
        let error = config(&[("MIMIC_MESH_OPAQUE_PORTS", value)]).expect_err(value);
        assert!(error.contains("MIMIC_MESH_OPAQUE_PORTS"), "{error}");
    }
}

#[test]
fn msh_71_connect_timeout_default_and_range() {
    assert_eq!(config(&[]).unwrap().connect_timeout, Duration::from_secs(2));
    assert_eq!(
        config(&[("MIMIC_MESH_CONNECT_TIMEOUT", "100ms")])
            .unwrap()
            .connect_timeout,
        Duration::from_millis(100)
    );
    assert_eq!(
        config(&[("MIMIC_MESH_CONNECT_TIMEOUT", "10s")])
            .unwrap()
            .connect_timeout,
        Duration::from_secs(10)
    );
    for value in ["99ms", "11s", "soon", "0s"] {
        let error = config(&[("MIMIC_MESH_CONNECT_TIMEOUT", value)]).expect_err(value);
        assert!(error.contains("MIMIC_MESH_CONNECT_TIMEOUT"), "{error}");
    }
}

#[test]
fn rty_05_defaults() {
    let config = config(&[]).unwrap();
    assert_eq!(config.retry_buffer, 64 * 1024);
    assert_eq!(config.retry_buffer_total, 8 * 1024 * 1024);
    assert_eq!(config.refusal_delay, Duration::from_millis(25));
}

#[test]
fn rty_05_values_are_read() {
    let config = config(&[
        ("MIMIC_MESH_RETRY_BUFFER", "1Mi"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "64Mi"),
    ])
    .unwrap();
    assert_eq!(config.retry_buffer, 1 << 20);
    assert_eq!(config.retry_buffer_total, 64 << 20);
    let config = self::config(&[
        ("MIMIC_MESH_RETRY_BUFFER", "0"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "1Mi"),
    ])
    .unwrap();
    assert_eq!(config.retry_buffer, 0, "0 is a valid buffer");
    assert_eq!(config.retry_buffer_total, 1 << 20);
    let config = self::config(&[
        ("MIMIC_MESH_RETRY_BUFFER", "1024Ki"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "2048Ki"),
    ])
    .unwrap();
    assert_eq!(config.retry_buffer, 1 << 20);
    assert_eq!(config.retry_buffer_total, 2 << 20);
}

#[test]
fn rty_05_invalid_values_are_refused() {
    for (var, value) in [
        ("MIMIC_MESH_RETRY_BUFFER", "1025Ki"),
        ("MIMIC_MESH_RETRY_BUFFER", "2Mi"),
        ("MIMIC_MESH_RETRY_BUFFER", "lots"),
        ("MIMIC_MESH_RETRY_BUFFER", "4M"),
        ("MIMIC_MESH_RETRY_BUFFER", "-1"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "1023Ki"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "0"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "65Mi"),
        ("MIMIC_MESH_RETRY_BUFFER_TOTAL", "lots"),
    ] {
        let error = config(&[(var, value)]).expect_err(&format!("{var}={value}"));
        assert!(error.contains(var), "{error}");
    }
}
