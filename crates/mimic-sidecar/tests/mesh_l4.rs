//! MSH-63 and MSH-64: L4 balancing of in-scope connections that are not HTTP (spec §7.9 "L4
//! balancing"): one pod per connection, the bytes untouched, a retry on another pod, then the
//! original destination.

mod common;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use common::mesh_pods::{Blackhole, EchoPod, cluster_ip, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Not HTTP, not TLS, not the HTTP/2 preface; includes `NUL` bytes.
const BINARY: &[u8] = b"\x00\x01binary\xff\x00protocol-bytes";

fn state_for(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(11),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// Sends `bytes` and reads them back.
async fn echoed(addr: SocketAddr, bytes: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(bytes).await.unwrap();
    let mut got = vec![0u8; bytes.len()];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut got))
        .await
        .expect("an echo in time")
        .expect("an echo");
    got
}

/// A TLS ClientHello offering TLS 1.1 at most, with this SNI.
fn hello(sni: &str) -> Vec<u8> {
    let mut ext = Vec::new();
    let host = sni.as_bytes();
    let mut list = vec![0u8];
    list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    list.extend_from_slice(host);
    let mut data = (list.len() as u16).to_be_bytes().to_vec();
    data.extend_from_slice(&list);
    ext.extend_from_slice(&[0, 0]);
    ext.extend_from_slice(&(data.len() as u16).to_be_bytes());
    ext.extend_from_slice(&data);
    let mut body = vec![0x03, 0x02];
    body.extend_from_slice(&[7u8; 32]);
    body.push(0);
    body.extend_from_slice(&[0, 2, 0x00, 0x2f]);
    body.extend_from_slice(&[1, 0]);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut handshake = vec![1, 0];
    handshake.extend_from_slice(&(body.len() as u16).to_be_bytes());
    handshake.extend_from_slice(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
    record.extend_from_slice(&handshake);
    record
}

async fn three() -> (EchoPod, EchoPod, EchoPod) {
    (
        EchoPod::start().await,
        EchoPod::start().await,
        EchoPod::start().await,
    )
}

#[tokio::test]
async fn msh_63_non_http_connections_are_spread_over_the_pods_and_echoed_unchanged() {
    let _logs = capture_logs();
    let (a, b, c) = three().await;
    let state = state_for(cluster_ip(), &[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for _ in 0..300 {
        assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    }
    for (name, pod) in [("a", &a), ("b", &b), ("c", &c)] {
        let n = pod.accepted();
        assert!((70..=130).contains(&n), "pod {name} got {n} of 300");
    }
}

#[tokio::test]
async fn msh_63_a_tls_client_hello_that_is_not_intercepted_reaches_a_pod_byte_for_byte() {
    let _logs = capture_logs();
    let (a, b, c) = three().await;
    let state = state_for(cluster_ip(), &[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let hello = hello("cart.shop.svc");
    for _ in 0..60 {
        assert_eq!(echoed(f.addr, &hello).await, hello);
    }
    let used = [&a, &b, &c].iter().filter(|p| p.accepted() > 0).count();
    assert_eq!(used, 3, "every pod got connections");
    for pod in [&a, &b, &c] {
        let received = pod.received();
        assert_eq!(received.len() % hello.len(), 0);
        assert!(received.chunks(hello.len()).all(|c| c == hello));
    }
}

#[tokio::test]
async fn msh_63_held_connections_spread_and_count_in_flight_while_they_last() {
    let _logs = capture_logs();
    let (a, b, c) = three().await;
    let state = state_for(cluster_ip(), &[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut held = Vec::new();
    for _ in 0..30 {
        let mut client = TcpStream::connect(f.addr).await.unwrap();
        client.write_all(BINARY).await.unwrap();
        let mut got = vec![0u8; BINARY.len()];
        client.read_exact(&mut got).await.unwrap();
        held.push(client);
    }
    for pod in [&a, &b, &c] {
        assert!(pod.accepted() <= 15, "a pod got {}", pod.accepted());
    }
    let metrics = f.metrics.encode();
    assert_eq!(
        counter(&metrics, "mimic_mesh_endpoint_in_flight"),
        30.0,
        "{metrics}"
    );
    drop(held);
    for _ in 0..100 {
        if counter(&f.metrics.encode(), "mimic_mesh_endpoint_in_flight") == 0.0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the connections still count in flight");
}

#[tokio::test]
async fn msh_63_the_access_line_of_a_connection_names_the_backend_and_the_pod() {
    let logs = capture_logs();
    let pod = EchoPod::start().await;
    let state = state_for(cluster_ip(), &[pod.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    let line = logs.access_exactly(1).await.remove(0);
    assert_eq!(line["backend"], "shop/cart:80", "{line}");
    assert_eq!(line["upstream"], pod.addr.to_string(), "{line}");
    assert_eq!(line["dst"], cluster_ip().to_string(), "{line}");
}

#[tokio::test]
async fn msh_63_opaque_ports_silent_clients_first_upgrades_and_stale_or_off_views_keep_the_relay() {
    let _logs = capture_logs();
    let (cluster, pod) = (EchoPod::start().await, EchoPod::start().await);

    // An opaque port.
    let state = state_for(cluster.addr, &[pod.addr], |c| {
        c.opaque_ports = BTreeSet::from([cluster.addr.port()]);
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    assert_eq!((cluster.accepted(), pod.accepted()), (1, 0), "opaque port");

    // A client silent for 50 ms, then speaking.
    let state = state_for(cluster.addr, &[pod.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.write_all(BINARY).await.unwrap();
    let mut got = vec![0u8; BINARY.len()];
    c.read_exact(&mut got).await.unwrap();
    assert_eq!(
        (cluster.accepted(), pod.accepted()),
        (2, 0),
        "silent client"
    );

    // A first request with Upgrade, then CONNECT.
    let upgrade: &[u8] = b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
    assert_eq!(echoed(f.addr, upgrade).await, upgrade);
    let connect: &[u8] = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
    assert_eq!(echoed(f.addr, connect).await, connect);
    assert_eq!(
        (cluster.accepted(), pod.accepted()),
        (4, 0),
        "first upgrade"
    );

    // A stale view.
    let state = state_for(cluster.addr, &[pod.addr], |c| {
        c.stale_after = Duration::from_millis(200);
    });
    let f = fixture_mesh(cluster.addr, &state, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    assert_eq!((cluster.accepted(), pod.accepted()), (5, 0), "stale view");

    // A view that says `"mesh": false`.
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster.addr, &[pod.addr], 0, false), "\"1-b\"");
    let f = fixture_mesh(cluster.addr, &state, None).await;
    assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    assert_eq!((cluster.accepted(), pod.accepted()), (6, 0), "mesh off");
}

#[tokio::test]
async fn msh_64_a_refusing_pod_of_two_is_skipped_without_a_fallback() {
    let _logs = capture_logs();
    let (cluster, live) = (EchoPod::start().await, EchoPod::start().await);
    let state = state_for(cluster.addr, &[closed_port().await, live.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    for _ in 0..30 {
        assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    }
    assert_eq!(live.accepted(), 30);
    assert_eq!(
        cluster.accepted(),
        0,
        "the original destination was not used"
    );
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        0.0
    );
}

#[tokio::test]
async fn msh_64_the_only_pod_refusing_falls_back_to_the_original_destination() {
    let _logs = capture_logs();
    let cluster = EchoPod::start().await;
    let state = state_for(cluster.addr, &[closed_port().await], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    for _ in 0..5 {
        assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    }
    assert_eq!(cluster.accepted(), 5);
    assert_eq!(
        counter(&f.metrics.encode(), "mimic_mesh_fallbacks_total"),
        5.0
    );
}

#[tokio::test]
async fn msh_64_with_the_original_destination_refusing_too_the_client_is_closed_without_a_byte() {
    let _logs = capture_logs();
    let (cluster, pod) = (closed_port().await, closed_port().await);
    let state = state_for(cluster, &[pod], |_| {});
    let f = fixture_mesh(cluster, &state, None).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(BINARY).await.unwrap();
    let mut got = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut got)).await;
    assert!(read.is_ok(), "the connection ends");
    assert!(got.is_empty(), "no byte");
}

#[tokio::test]
async fn msh_64_a_pod_that_closes_after_reading_is_not_retried() {
    let _logs = capture_logs();
    let (cluster, closer, other) = (
        EchoPod::start().await,
        EchoPod::closer().await,
        EchoPod::start().await,
    );
    let state = state_for(cluster.addr, &[closer.addr], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(BINARY).await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut got))
        .await
        .expect("the connection ends")
        .ok();
    assert!(got.is_empty());
    assert_eq!(closer.accepted(), 1);
    assert_eq!(closer.received(), BINARY, "the bytes reached the pod once");
    assert_eq!((cluster.accepted(), other.accepted()), (0, 0));
}

#[tokio::test]
async fn msh_68_l4_connections_avoid_a_blackholed_pod() {
    let _logs = capture_logs();
    let hole = Blackhole::new().await;
    let live = EchoPod::start().await;
    let state = state_for(cluster_ip(), &[hole.addr, live.addr], |c| {
        c.connect_timeout = Duration::from_millis(300);
        c.avoid_failed_for = Duration::from_secs(30);
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let began = std::time::Instant::now();
    for _ in 0..30 {
        assert_eq!(echoed(f.addr, BINARY).await, BINARY);
    }
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "30 connections took {:?}",
        began.elapsed()
    );
    assert_eq!(live.accepted(), 30);
}
