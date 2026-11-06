//! EGR-11 (plain TCP and `CONNECT` tunnels; the TLS reasons are in `tls_intercept.rs`) and
//! EGR-16 (metrics and logs of outbound discovery), in process.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{capture_logs, counted, echo_server, fixture_discovery};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

const PROXY_SECRET: &str = "CANARY-PROXYAUTH-61c";
const PATH_SECRET: &str = "CANARY-PATH-61c";
const QUERY_SECRET: &str = "CANARY-QUERY-61c";
const HEADER_SECRET: &str = "CANARY-HEADER-61c";
const BODY_SECRET: &str = "CANARY-BODY-61c";
const SECRETS: [&str; 5] = [
    PROXY_SECRET,
    PATH_SECRET,
    QUERY_SECRET,
    HEADER_SECRET,
    BODY_SECRET,
];

fn discovery(config: &DiscoveryConfig) -> Discovery {
    Discovery::start(config, Some("pod-a".into())).unwrap()
}

fn outbound_scopes(discovery: &Discovery) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_str(&discovery.report().to_json().unwrap()).unwrap();
    json["scopes"]["outbound"].clone()
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..250 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

/// Sends opaque bytes through `proxy` to an echo server and reads them back.
async fn tcp_round_trip(proxy: SocketAddr) {
    let mut c = TcpStream::connect(proxy).await.unwrap();
    let bytes = [0u8, 1, 2, 3, 250, 251];
    c.write_all(&bytes).await.unwrap();
    let mut back = [0u8; 6];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut back))
        .await
        .expect("echoed in time")
        .unwrap();
    assert_eq!(back, bytes);
}

/// An egress proxy: `CONNECT deny.*` is answered `403`, any other `CONNECT` `200` and then
/// echoed.
async fn egress_proxy() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                if String::from_utf8_lossy(&head).starts_with("CONNECT deny.") {
                    let _ = s
                        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                if s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn connect_through(proxy: SocketAddr, target: &str, extra: &str) -> String {
    let mut c = TcpStream::connect(proxy).await.unwrap();
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{extra}\r\n");
    c.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut buf = [0u8; 512];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(n > 0, "closed before the head");
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8(head).unwrap();
    if head.starts_with("HTTP/1.1 200") {
        // Through the tunnel: the bytes are echoed unchanged.
        c.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        tokio::time::timeout(TIMEOUT, c.read_exact(&mut back))
            .await
            .expect("echoed in time")
            .unwrap();
        assert_eq!(&back, b"ping");
    }
    head
}

#[tokio::test]
async fn egr_11_tcp_connections_are_counted_per_destination_with_no_endpoint() {
    let _logs = capture_logs();
    let echo = echo_server().await;
    let discovery = discovery(&DiscoveryConfig::default());
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(echo)).await;
    tcp_round_trip(out.addr).await;
    tcp_round_trip(out.addr).await;
    counted(
        &out.metrics,
        "mimic_discovery_tunnels_total{kind=\"tcp\",result=\"recorded\"} 2",
    )
    .await;
    let scopes = outbound_scopes(&discovery);
    let key = format!("127.0.0.1:{}", echo.port());
    assert_eq!(scopes[&key]["tunnels"], serde_json::json!({"tcp": 2}));
    assert!(discovery.report().endpoints().is_empty());
    assert_eq!(discovery.report().sample_bytes(), 0);
    // Inbound connections are not tunnels of discovery.
    let inbound = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(echo)).await;
    tcp_round_trip(inbound.addr).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(scopes, outbound_scopes(&discovery));
}

#[tokio::test]
async fn egr_11_a_connect_answered_2xx_is_a_tunnel_under_the_target_authority() {
    let _logs = capture_logs();
    let proxy = egress_proxy().await;
    let discovery = discovery(&DiscoveryConfig::default());
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(proxy)).await;
    let head = connect_through(out.addr, "Target.Example:443", "").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let head = connect_through(out.addr, "deny.example:443", "").await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    counted(
        &out.metrics,
        "mimic_discovery_tunnels_total{kind=\"connect\",result=\"recorded\"} 1",
    )
    .await;
    // Let a wrongly recorded refusal arrive before looking.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let scopes = outbound_scopes(&discovery);
    assert_eq!(scopes.as_object().unwrap().len(), 1, "{scopes}");
    assert_eq!(
        scopes["target.example:443"]["tunnels"],
        serde_json::json!({"connect": 1})
    );
    assert!(discovery.report().endpoints().is_empty());
    // A `CONNECT` is neither an invalid exchange nor a recorded one.
    let text = out.metrics.encode();
    assert!(!text.contains("mimic_discovery_exchanges_total{direction=\"outbound\""));
}

#[tokio::test]
async fn egr_11_the_authority_limit_refuses_new_tunnels_and_exchanges_with_one_warning() {
    let logs = capture_logs();
    let config = DiscoveryConfig {
        max_authorities: 2,
        ..DiscoveryConfig::default()
    };
    let discovery = discovery(&config);
    let mut sidecars = Vec::new();
    let mut first = None;
    for _ in 0..3 {
        let echo = echo_server().await;
        first.get_or_insert(echo);
        let f = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(echo)).await;
        tcp_round_trip(f.addr).await;
        sidecars.push(f);
    }
    // A known authority still counts.
    let again = fixture_discovery(Direction::Outbound, &discovery, move |_| first).await;
    tcp_round_trip(again.addr).await;
    let metrics = &sidecars[0].metrics;
    counted(
        metrics,
        "mimic_discovery_tunnels_total{kind=\"tcp\",result=\"recorded\"} 3",
    )
    .await;
    counted(
        metrics,
        "mimic_discovery_tunnels_total{kind=\"tcp\",result=\"authority_limit\"} 1",
    )
    .await;
    counted(metrics, "mimic_discovery_authorities 2").await;
    // An exchange to a new authority is refused as well, and one line says so.
    let provider = echo_server().await;
    let http = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(provider)).await;
    let mut c = TcpStream::connect(http.addr).await.unwrap();
    c.write_all(b"GET /x HTTP/1.1\r\nHost: third.example\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    counted(
        metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"authority_limit\"} 1",
    )
    .await;
    logs.settled().await;
    let warnings: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|l| l["message"] == "authority limit reached")
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["level"], "WARN");
    assert_eq!(warnings[0]["limit"], 2);
    assert_eq!(outbound_scopes(&discovery).as_object().unwrap().len(), 2);
}

/// A provider that answers every request `200` with a small JSON body.
async fn provider() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let body = "{\"ok\":true}";
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if s.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn get(proxy: SocketAddr, host: &str, path: &str) {
    let mut c = TcpStream::connect(proxy).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut out))
        .await
        .expect("answered in time")
        .unwrap();
    assert!(out.starts_with(b"HTTP/1.1 200"));
}

fn lines(logs: &common::Logs) -> Vec<serde_json::Value> {
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test]
async fn egr_16_template_absorbed_is_scoped_to_its_authority_and_keeps_the_port() {
    let logs = capture_logs();
    let provider = provider().await;
    let config = DiscoveryConfig {
        max_literals: 3,
        ..DiscoveryConfig::default()
    };
    let discovery = discovery(&config);
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(provider)).await;
    get(out.addr, "b.example:443", "/users/zed").await;
    for name in ["ann", "bob", "cyd", "dee"] {
        get(out.addr, "a.example:443", &format!("/users/{name}")).await;
    }
    counted(
        &out.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 5",
    )
    .await;
    logs.settled().await;
    let all = lines(&logs);
    let absorbed: Vec<_> = all
        .iter()
        .filter(|l| l["message"] == "template absorbed")
        .collect();
    assert_eq!(absorbed.len(), 1, "{all:?}");
    let line = absorbed[0];
    assert_eq!(line["direction"], "outbound");
    assert_eq!(line["authority"], "a.example:443");
    assert_eq!(line["port"], provider.port());
    assert_eq!(line["template"], "/users/{users_id}");
    let old = line["old"].as_str().unwrap();
    assert!(old.contains("/users/ann") && !old.contains("zed"), "{old}");
    // One INFO per new outbound endpoint, with the authority and no port.
    let discovered: Vec<_> = all
        .iter()
        .filter(|l| l["message"] == "endpoint discovered")
        .collect();
    assert!(!discovered.is_empty());
    for l in discovered {
        assert!(l["authority"].is_string(), "{l}");
        assert!(l.get("port").is_none(), "{l}");
    }
}

#[tokio::test]
async fn egr_16_authorities_gauge_and_series_are_bounded_and_carry_no_recorded_value() {
    let logs = capture_logs();
    let provider = provider().await;
    let proxy = egress_proxy().await;
    let echo = echo_server().await;
    let discovery = discovery(&DiscoveryConfig::default());
    let http = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(provider)).await;
    let tunnel = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(proxy)).await;
    let raw = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(echo)).await;
    for n in 0..3 {
        get(
            http.addr,
            &format!("h{n}.example:443"),
            &format!("/{PATH_SECRET}-{n}?token={QUERY_SECRET}"),
        )
        .await;
    }
    let extra =
        format!("Proxy-Authorization: Basic {PROXY_SECRET}\r\nX-Token: {HEADER_SECRET}\r\n");
    let head = connect_through(tunnel.addr, "tun.example:443", &extra).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    tcp_round_trip(raw.addr).await;
    // 3 exchange authorities, the `CONNECT` target and the raw destination.
    counted(&http.metrics, "mimic_discovery_authorities 5").await;
    counted(
        &http.metrics,
        "mimic_discovery_tunnels_total{kind=\"connect\",result=\"recorded\"} 1",
    )
    .await;
    let text = http.metrics.encode();
    let series: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("mimic_discovery_"))
        .collect();
    for line in &series {
        assert!(!line.contains("authority="), "{line}");
        assert!(!line.contains("example"), "{line}");
    }
    let tunnels = series
        .iter()
        .filter(|l| l.starts_with("mimic_discovery_tunnels_total{"))
        .count();
    assert!(tunnels <= 8 * 3, "{tunnels}");
    logs.settled().await;
    let log = logs.text();
    let report = discovery.report().to_json().unwrap();
    for (i, secret) in SECRETS.iter().enumerate() {
        assert!(!text.contains(secret), "secret {i} in the metrics");
        assert!(!log.contains(secret), "secret {i} in the logs");
        assert!(!report.contains(secret), "secret {i} in the report");
    }
    // Tunnels carry no path, header or sample of their own.
    let scopes = outbound_scopes(&discovery);
    assert_eq!(
        scopes["tun.example:443"]["tunnels"],
        serde_json::json!({"connect": 1})
    );
    assert!(scopes["tun.example:443"].get("endpoints").is_none());
    eventually("the recorder is idle", || {
        discovery.report().authorities() == 5
    })
    .await;
}
