//! MSH-58 to MSH-60: opaque ports and server-first protocols, connections the pod closed, and
//! `Upgrade` and `CONNECT` in the middle of a mesh-routed HTTP/1 connection.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::mesh_pods::{Counts, H1Client, Pod, view_on};
use common::{capture_logs, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BANNER: &[u8] = b"220 mimic test banner\r\n";

fn state_for(
    cluster: SocketAddr,
    pods: &[SocketAddr],
    configure: impl FnOnce(&mut MeshConfig),
) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(5),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

/// A server that speaks first, then echoes.
async fn banner_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                if s.write_all(BANNER).await.is_err() {
                    return;
                }
                let mut buf = [0u8; 1024];
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

async fn server_first(opaque: bool) {
    let _logs = capture_logs();
    let server = banner_server().await;
    let pod = Pod::http1("never", 0).await;
    let state = state_for(server, &[pod.addr], |c| {
        if opaque {
            c.opaque_ports.insert(server.port());
        }
    });
    let f = fixture_mesh(server, &state, None).await;
    let began = Instant::now();
    let mut client = TcpStream::connect(f.addr).await.unwrap();
    let mut banner = vec![0u8; BANNER.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut banner))
        .await
        .expect("the banner in time")
        .unwrap();
    assert_eq!(banner, BANNER);
    assert!(
        began.elapsed() < Duration::from_millis(300),
        "the banner took {:?}",
        began.elapsed()
    );
    client.write_all(b"hello\r\n").await.unwrap();
    let mut echo = [0u8; 7];
    client.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"hello\r\n");
    assert_eq!(pod.counts.requests(), 0, "no pod of the view was used");
}

#[tokio::test]
async fn msh_58_a_server_first_protocol_on_an_opaque_port_has_its_banner_at_once() {
    server_first(true).await;
}

#[tokio::test]
async fn msh_58_a_server_first_protocol_on_another_port_does_not_wait_a_second() {
    server_first(false).await;
}

/// Answers every request `200` with `ok` and closes the connection right after (`idle`: after
/// 50 ms without a request instead).
async fn closing_pod(idle: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    let read = if idle {
                        match tokio::time::timeout(Duration::from_millis(50), s.read(&mut buf))
                            .await
                        {
                            Ok(r) => r,
                            Err(_) => return,
                        }
                    } else {
                        s.read(&mut buf).await
                    };
                    if !matches!(read, Ok(n) if n > 0) {
                        return;
                    }
                    let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    if s.write_all(answer).await.is_err() || !idle {
                        return;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn msh_59_requests_succeed_against_pods_that_close_their_connections() {
    let _logs = capture_logs();
    for idle in [false, true] {
        let pod = closing_pod(idle).await;
        let state = state_for(common::mesh_pods::cluster_ip(), &[pod], |_| {});
        let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
        let mut client = H1Client::connect(f.addr).await;
        for n in 0..200 {
            let (head, body) = client.get(&format!("/{n}")).await;
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            assert_eq!(body, "ok");
            if idle && n % 50 == 0 {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
    }
}

/// A pod that reads each request head and closes without answering; the count is of heads.
async fn silent_closer() -> (SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = heads.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        return;
                    }
                    pending.extend_from_slice(&buf[..n]);
                    if pending.windows(4).any(|w| w == b"\r\n\r\n") {
                        seen.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                }
            });
        }
    });
    (addr, heads)
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

#[tokio::test]
async fn msh_59_a_request_the_pod_read_is_not_sent_again() {
    let _logs = capture_logs();
    let (pod, heads) = silent_closer().await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"POST /x HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 2\r\n\r\nab")
        .await;
    assert!(
        client.read().await.is_none(),
        "the connection is closed, no answer"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        heads.load(Ordering::SeqCst),
        1,
        "the pod saw the request once"
    );
    let metrics = f.metrics.encode();
    assert_eq!(
        counter(&metrics, "mimic_mesh_reconnects_total"),
        0.0,
        "reconnects"
    );
    let none = "mimic_mesh_requests_total{service=\"shop/cart\",route=\"none\",backend=\"shop/cart:80\",status_class=\"none\"";
    assert_eq!(counter(&metrics, none), 1.0, "requests without a response");
}

#[tokio::test]
async fn msh_59_an_unwritten_request_is_sent_again_once_on_a_new_http1_connection() {
    let _logs = capture_logs();
    let pod = Pod::http1("a", 0).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod.addr], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    state.poison_next_checkout();
    let (head, body) = client.get("/two").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "a");
    assert_eq!(
        pod.counts.requests(),
        2,
        "each request reached the pod once"
    );
    let reconnects = counter(&f.metrics.encode(), "mimic_mesh_reconnects_total");
    assert_eq!(reconnects, 1.0, "reconnects");
    // The pooled connection, the one handed out closed, and the new one.
    settled(|| pod.counts.accepted() == 3).await;
}

#[tokio::test]
async fn msh_59_an_unwritten_request_is_sent_again_once_on_a_new_h2_connection() {
    let _logs = capture_logs();
    let pod = Pod::h2c("a", 0, None).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod.addr], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    assert_eq!(common::mesh_pods::h2c_calls(f.addr, 1, 1).await, vec!["a"]);
    state.poison_next_checkout();
    assert_eq!(common::mesh_pods::h2c_calls(f.addr, 1, 1).await, vec!["a"]);
    assert_eq!(
        pod.counts.requests(),
        2,
        "each request reached the pod once"
    );
    let reconnects = counter(&f.metrics.encode(), "mimic_mesh_reconnects_total");
    assert_eq!(reconnects, 1.0, "reconnects");
    // The first connection, the one handed out closed, and a new one (not the first again).
    settled(|| pod.counts.accepted() == 3).await;
}

/// A pod that answers `200 ok` to a request and, to one with `Upgrade` (or `CONNECT`), `101`
/// (`200`) and then echoes; `decline` answers `200` to those too.
async fn tunnel_pod(
    decline: bool,
) -> (
    SocketAddr,
    Counts,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counts = Counts::default();
    let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (shared, log) = (counts.clone(), heads.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let (counts, log) = (shared.clone(), log.clone());
            counts.accepted.fetch_add(1, Ordering::SeqCst);
            counts.open.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut pending = Vec::new();
                let mut buf = [0u8; 4096];
                'conn: while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&pending[..p]).into_owned();
                        pending.drain(..p + 4);
                        let lower = head.to_lowercase();
                        log.lock().unwrap().push(head.clone());
                        let connect = head.starts_with("CONNECT");
                        if (connect || lower.contains("upgrade: websocket")) && !decline {
                            let reply: &[u8] = if connect {
                                b"HTTP/1.1 200 Connection established\r\n\r\n"
                            } else {
                                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: abc\r\n\r\n"
                            };
                            if s.write_all(reply).await.is_err() {
                                break 'conn;
                            }
                            // Tunnel: echo what comes, starting with what was already read.
                            if !pending.is_empty() && s.write_all(&pending).await.is_err() {
                                break 'conn;
                            }
                            while let Ok(n) = s.read(&mut buf).await {
                                if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            break 'conn;
                        }
                        counts.requests.fetch_add(1, Ordering::SeqCst);
                        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                        if s.write_all(ok).await.is_err() {
                            break 'conn;
                        }
                    }
                }
                counts.open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (addr, counts, heads)
}

async fn settled(f: impl Fn() -> bool) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("not settled");
}

#[tokio::test]
async fn msh_60_a_websocket_upgrade_after_a_request_becomes_a_tunnel_to_a_pod() {
    let logs = capture_logs();
    let (pod, counts, heads) = tunnel_pod(false).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = client.get("/one").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "ok");
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 101");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let lower = head.to_lowercase();
    assert!(
        lower.contains("upgrade: websocket") && lower.contains("sec-websocket-accept: abc"),
        "{head}"
    );
    client.send(b"ping\x00\x01binary").await;
    let echoed = client.read_raw(12).await;
    assert_eq!(echoed, b"ping\x00\x01binary");
    let seen = heads
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap()
        .to_lowercase();
    assert!(
        seen.contains("upgrade: websocket") && seen.contains("sec-websocket-key: k"),
        "{seen}"
    );
    drop(client);
    settled(|| counts.open.load(Ordering::SeqCst) == 0).await;
    let lines = logs.access_exactly(2).await;
    assert_eq!(lines[1]["upgrade"], true, "the access line of the upgrade");
    assert_eq!(lines[1]["status"], 101);
    // The pod's connection was not pooled: the next request opens another.
    let before = counts.accepted.load(Ordering::SeqCst);
    H1Client::connect(f.addr).await.get("/after").await;
    assert_eq!(counts.accepted.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn msh_60_connect_after_a_request_becomes_a_tunnel_and_a_declined_upgrade_stays_http() {
    let _logs = capture_logs();
    let (pod, counts, _) = tunnel_pod(false).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    client
        .send(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 200");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    client.send(b"tunnelled").await;
    assert_eq!(client.read_raw(9).await, b"tunnelled");
    drop(client);
    settled(|| counts.open.load(Ordering::SeqCst) == 0).await;

    let (pod, _, _) = tunnel_pod(true).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/one").await;
    client
        .send(b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 200");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let (head, body) = client.get("/still-http").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "ok", "{head}");
}

#[tokio::test]
async fn msh_60_an_h2_connect_is_answered_501() {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let _logs = capture_logs();
    let pod = Pod::h2c("a", 0, None).await;
    let state = state_for(common::mesh_pods::cluster_ip(), &[pod.addr], |_| {});
    let f = fixture_mesh(common::mesh_pods::cluster_ip(), &state, None).await;
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = hyper::Request::connect("example.com:443")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = sender.send_request(request).await.expect("an answer");
    assert_eq!(response.status(), 501);
    assert_eq!(response.headers()["x-mimic-error"], "unsupported");
}
