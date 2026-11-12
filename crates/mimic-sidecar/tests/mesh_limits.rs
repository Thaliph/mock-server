//! MSH-53: the accepted limits of mesh-routed HTTP/1 (spec §4.1 "Mesh-routed HTTP"), as built.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::mesh_pods::{H1Client, Pod, RawPod, cluster_ip, view, view_on};
use common::{capture_logs, closed_port, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn state_for(cluster: SocketAddr, pods: &[SocketAddr]) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(3),
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
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

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_lowercase()
}

#[tokio::test]
async fn msh_53_header_case_is_kept_and_repeated_fields_keep_their_order() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let state = state_for(cluster_ip(), &[pod.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /x HTTP/1.1\r\nhOsT: cart.shop.svc\r\nX-Dup: one\r\nZ-Last: z\r\nx-dup: two\r\nX-DUP: three\r\n\r\n")
        .await;
    client.read().await.unwrap();
    let got = String::from_utf8(pod.received()).unwrap();
    let (one, two, three) = (
        got.find("X-Dup: one").expect(&got),
        got.find("x-dup: two").expect(&got),
        got.find("X-DUP: three").expect(&got),
    );
    assert!(one < two && two < three, "{got}");
    assert!(got.contains("\r\nhOsT: cart.shop.svc\r\n"), "{got}");
    assert!(got.contains("\r\nZ-Last: z\r\n"), "{got}");
}

#[tokio::test]
async fn msh_53_hop_by_hop_fields_are_consumed_and_connection_close_closes_the_app_side_only() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let state = state_for(cluster_ip(), &[pod.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /hop HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: close, X-Hop\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nX-Hop: 1\r\nTE: deflate\r\nX-Kept: yes\r\n\r\n")
        .await;
    let (head, body) = client.read().await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, b"/hop");
    assert!(client.read().await.is_none(), "the app side is closed");
    let seen = lower(&pod.received());
    for gone in [
        "connection:",
        "keep-alive:",
        "proxy-connection:",
        "x-hop:",
        "te:",
    ] {
        assert!(!seen.contains(gone), "{gone} reached the pod:\n{seen}");
    }
    assert!(seen.contains("x-kept: yes"), "{seen}");
    // The connection to the pod went back to the pool: another client uses it.
    let mut other = H1Client::connect(f.addr).await;
    other.get("/again").await;
    assert_eq!(pod.counts.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn msh_53_pipelined_requests_are_answered_in_order() {
    let _logs = capture_logs();
    let (a, b) = (
        RawPod::start(echo_path).await,
        RawPod::start(echo_path).await,
    );
    let state = state_for(cluster_ip(), &[a.addr, b.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut burst = Vec::new();
    for n in 0..5 {
        burst.extend_from_slice(
            format!("GET /{n} HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n").as_bytes(),
        );
    }
    client.send(&burst).await;
    for n in 0..5 {
        let (_, body) = client.read().await.unwrap();
        assert_eq!(body, format!("/{n}").into_bytes());
    }
}

#[tokio::test]
async fn msh_53_a_request_hyper_rejects_is_answered_400_and_the_connection_closes() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let state = state_for(cluster_ip(), &[pod.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    for bad in [
        &b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nBad Header: 1\r\n\r\n"[..],
        b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
    ] {
        let mut client = H1Client::connect(f.addr).await;
        client.send(bad).await;
        let (head, _) = client.read().await.expect("an answer");
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(client.read().await.is_none());
    }
    assert_eq!(
        pod.counts.requests.load(Ordering::SeqCst),
        0,
        "nothing reached the pod"
    );
}

#[tokio::test]
async fn msh_53_the_answers_of_the_mesh_itself() {
    let logs = capture_logs();
    // A pod that refuses connections.
    let refused = closed_port().await;
    let state = state_for(cluster_ip(), &[refused]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = {
        client
            .send(b"GET / HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n")
            .await;
        client.read().await.unwrap()
    };
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        lower(head.as_bytes()).contains("x-mimic-error: connect-failed"),
        "{head}"
    );
    assert!(
        lower(head.as_bytes()).contains("content-length: 0"),
        "{head}"
    );
    assert!(body.is_empty());
    // The connection stays: a pod that comes up is used by the next request.
    state.hold(view(&[], 0), "\"1-b\"");
    let (head, _) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        lower(head.as_bytes()).contains("x-mimic-error: no-endpoint"),
        "{head}"
    );
    let pod = Pod::http1("up", 0).await;
    state.hold(view(&[pod.addr], 0), "\"1-c\"");
    let (head, body) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "up");
    let text = f.metrics.encode();
    assert!(
        text.contains("status_class=\"5xx\",reason=\"connect-failed\"} 1"),
        "the metrics"
    );
    assert!(
        text.contains("status_class=\"5xx\",reason=\"no-endpoint\"} 1"),
        "the metrics"
    );
    drop(logs);

    // A response head hyper rejects.
    let bad = RawPod::start(|_| {
        b"HTTP/1.1 200 OK\r\nBad Header: 1\r\nContent-Length: 0\r\n\r\n".to_vec()
    })
    .await;
    let state = state_for(cluster_ip(), &[bad.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/").await;
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
    assert!(
        lower(head.as_bytes()).contains("x-mimic-error: invalid-response"),
        "{head}"
    );

    // `obs-fold` in a response head is unfolded to one space.
    let folded = RawPod::start(|_| {
        b"HTTP/1.1 200 OK\r\nX-Fold: a\r\n   b\r\nContent-Length: 0\r\n\r\n".to_vec()
    })
    .await;
    let state = state_for(cluster_ip(), &[folded.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert!(lower(head.as_bytes()).contains("x-fold: a b"), "{head}");
}

async fn exchange(addr: SocketAddr, request: &[u8], expect: usize) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(request).await.unwrap();
    let mut got = vec![0u8; expect];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut got))
        .await
        .expect("answered in time")
        .unwrap();
    got
}

#[tokio::test]
async fn msh_53_a_first_upgrade_or_connect_request_keeps_the_byte_relay() {
    let _logs = capture_logs();
    const SWITCH: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    const CONNECTED: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";
    let destination = RawPod::start(|head| {
        if head.starts_with(b"CONNECT") {
            CONNECTED
        } else {
            SWITCH
        }
        .to_vec()
    })
    .await;
    let pod = Pod::http1("never", 0).await;
    let state = state_for(destination.addr, &[pod.addr]);
    let f = fixture_mesh(destination.addr, &state, None).await;
    let upgrade = b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: x\r\n\r\n";
    assert_eq!(exchange(f.addr, upgrade, SWITCH.len()).await, SWITCH);
    let connect = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
    assert_eq!(exchange(f.addr, connect, CONNECTED.len()).await, CONNECTED);
    let mut sent = upgrade.to_vec();
    sent.extend_from_slice(connect);
    assert_eq!(
        destination.received(),
        sent,
        "the destination got the bytes as sent"
    );
    assert_eq!(pod.counts.requests(), 0, "no pod of the view was used");
}

#[tokio::test]
async fn msh_53_a_later_upgrade_request_on_a_mesh_routed_connection_is_answered_501() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let state = state_for(cluster_ip(), &[pod.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    client.get("/two").await;
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 501"), "{head}");
    assert!(
        lower(head.as_bytes()).contains("x-mimic-error: unsupported"),
        "{head}"
    );
    assert!(client.read().await.is_none(), "the connection closes");
    assert_eq!(pod.counts.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn msh_53_content_length_with_chunked_is_forwarded_chunked_only() {
    let _logs = capture_logs();
    let pod = RawPod::start(echo_path).await;
    let state = state_for(cluster_ip(), &[pod.addr]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"POST / HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let seen = lower(&pod.received());
    assert!(seen.contains("transfer-encoding: chunked"), "{seen}");
    assert!(!seen.contains("content-length"), "{seen}");
}
