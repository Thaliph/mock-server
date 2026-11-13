//! MSH-65: the head reader of a mesh-routed HTTP/1 connection refuses what `hyper` would
//! reject with the sidecar's own `400`, and lets `Upgrade` and `CONNECT` leave `hyper` with
//! their head as received (spec §4.1 limits 1 and 8).

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::mesh_pods::{H1Client, RawPod, cluster_ip, view_on};
use common::{capture_logs, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};

fn state_for(pods: &[SocketAddr]) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(3),
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster_ip(), pods, 0, true), "\"1-a\"");
    state
}

/// Answers `200` with the path of the request as its body.
fn echo_path(head: &[u8]) -> Vec<u8> {
    let line = String::from_utf8_lossy(head);
    let path = line.split(' ').nth(1).unwrap_or("?").to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{path}",
        path.len()
    )
    .into_bytes()
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

const BAD: [&[u8]; 6] = [
    b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nBad Header: 1\r\n\r\n",
    b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nX: a\rb\r\n\r\n",
    b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nX: a\r\n  folded\r\n\r\n",
    b"POST / HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
    b"POST / HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: gzip\r\n\r\n",
    b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nX: a\0b\r\n\r\n",
];

fn assert_refused(head: &str) {
    let lower = head.to_lowercase();
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    assert!(lower.contains("x-mimic-error: invalid-request"), "{head}");
    assert!(lower.contains("content-length: 0"), "{head}");
    assert!(lower.contains("connection: close"), "{head}");
}

#[tokio::test]
async fn msh_65_a_refused_head_is_answered_by_the_sidecar_and_never_reaches_a_pod() {
    let logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    for bad in BAD {
        let mut client = H1Client::connect(f.addr).await;
        client.send(bad).await;
        let (head, _) = client.read().await.expect("an answer");
        assert_refused(&head);
        assert!(client.read().await.is_none(), "the connection closes");
    }
    assert_eq!(pod.counts.requests.load(Ordering::SeqCst), 0);
    assert!(pod.received().is_empty(), "no byte reached the pod");
    let lines = logs.access_exactly(BAD.len()).await;
    for line in &lines {
        assert_eq!(line["status"], 400, "{line}");
        assert_eq!(line["result"], "unframed", "{line}");
    }
    let metrics = f.metrics.encode();
    let refused = "mimic_mesh_requests_total{service=\"shop/cart\",route=\"none\",backend=\"shop/cart:80\",status_class=\"4xx\",reason=\"invalid-request\"}";
    assert_eq!(counter(&metrics, refused), BAD.len() as f64, "{metrics}");
}

#[tokio::test]
async fn msh_65_a_head_over_64_kib_is_refused() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut huge = b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nX: ".to_vec();
    huge.extend(std::iter::repeat_n(b'a', 70 * 1024));
    huge.extend_from_slice(b"\r\n\r\n");
    client.send(&huge).await;
    let (head, _) = client.read().await.expect("an answer");
    assert_refused(&head);
    assert!(pod.received().is_empty());
}

#[tokio::test]
async fn msh_65_a_head_cut_at_every_byte_is_refused_the_same() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    for byte in BAD[0] {
        client.send(&[*byte]).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let (head, _) = client.read().await.expect("an answer");
    assert_refused(&head);
    assert!(pod.received().is_empty());
}

#[tokio::test]
async fn msh_65_a_refused_request_behind_two_good_ones_is_answered_after_them() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut burst = Vec::new();
    for n in 0..2 {
        burst.extend_from_slice(
            format!("GET /{n} HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n").as_bytes(),
        );
    }
    burst.extend_from_slice(BAD[0]);
    client.send(&burst).await;
    for n in 0..2 {
        let (head, body) = client.read().await.expect("an answer");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, format!("/{n}").into_bytes());
    }
    let (head, _) = client.read().await.expect("the refusal");
    assert_refused(&head);
    assert!(client.read().await.is_none());
    assert_eq!(pod.counts.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn msh_65_content_length_with_chunked_is_still_forwarded_chunked() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"POST / HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let seen = String::from_utf8_lossy(&pod.received()).to_lowercase();
    assert!(seen.contains("transfer-encoding: chunked"), "{seen}");
    assert!(!seen.contains("content-length"), "{seen}");
    // The reader cannot follow such a request: a later `Upgrade` is not served.
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("an answer");
    let lower = head.to_lowercase();
    assert!(head.starts_with("HTTP/1.1 501"), "{head}");
    assert!(lower.contains("x-mimic-error: unsupported"), "{head}");
}

/// What the pod answers to the upgrade request below.
const SWITCH: &[u8] =
    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";

/// Not in the order `hyper` would write it: `X-A` twice around `X-B`, a lower-case `host`.
const UPGRADE: &[u8] = b"GET /ws HTTP/1.1\r\nhost: cart.shop.svc\r\nX-A: 1\r\nUpgrade: websocket\r\nX-B: 2\r\nX-A: 3\r\nConnection: Upgrade\r\nSec-WebSocket-Key: k\r\n\r\n";

async fn upgrade_pod() -> RawPod {
    RawPod::start(|head| {
        if head.starts_with(b"GET /ws") {
            SWITCH.to_vec()
        } else {
            echo_path(head)
        }
    })
    .await
}

#[tokio::test]
async fn msh_65_an_upgrade_after_pipelined_requests_reaches_the_pod_as_received() {
    let logs = capture_logs();
    let pod = upgrade_pod().await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    let first = b"GET /0 HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n";
    let mut burst = first.to_vec();
    burst.extend_from_slice(UPGRADE);
    client.send(&burst).await;
    let (head, body) = client.read().await.expect("the first answer");
    assert!(head.starts_with("HTTP/1.1 200") && body == b"/0", "{head}");
    let (head, _) = client.read().await.expect("the 101");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let seen = pod.received();
    assert!(
        seen.ends_with(UPGRADE),
        "the pod got the head as received: {}",
        String::from_utf8_lossy(&seen)
    );
    // Bytes now flow untouched, both ways.
    client.send(b"frame").await;
    drop(client);
    let lines = logs.access_exactly(2).await;
    assert_eq!(lines[1]["upgrade"], true, "{}", lines[1]);
    assert_eq!(lines[1]["status"], 101, "{}", lines[1]);
}

#[tokio::test]
async fn msh_65_an_upgrade_head_cut_across_reads_reaches_the_pod_as_received() {
    let _logs = capture_logs();
    let pod = upgrade_pod().await;
    let f = fixture_mesh(cluster_ip(), &state_for(&[pod.addr]), None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    for part in UPGRADE.chunks(23) {
        client.send(part).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (head, _) = client.read().await.expect("the 101");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(pod.received().ends_with(UPGRADE));
}
