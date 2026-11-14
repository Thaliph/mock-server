//! RTY-27 to RTY-29: the inbound side of a meshed sidecar answers `503` `connect-failed` when
//! its app refuses the connection (spec §4.1 "Refused app connections"); everything else keeps
//! the reset. An inbound sidecar in process in front of a closed port.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::{capture_logs, closed_port};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::red::ProxyMetrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Rig {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    /// Where the sidecar's connect goes (the pod address the caller dialled).
    target: Arc<Mutex<SocketAddr>>,
}

/// An inbound listener whose app is at `target`; `mesh` is `None` for `MIMIC_MESH=off`.
async fn start(target: SocketAddr, mesh: Option<&MeshState>) -> Rig {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel = TunnelMetrics::register(&mut metrics);
    let target = Arc::new(Mutex::new(target));
    let at = target.clone();
    let original: OriginalDst = Arc::new(move |_| Ok(*at.lock().unwrap()));
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let mut shared = Shared::new(vec![addr], tunnel, original).with_proxy_metrics(proxy);
    if let Some(state) = mesh {
        state.register(&mut metrics);
        shared = shared.with_mesh(state.clone());
    }
    spawn_listener(listener, Direction::Inbound, Arc::new(shared));
    Rig {
        addr,
        metrics: Arc::new(metrics),
        target,
    }
}

impl Rig {
    fn refusals(&self) -> Option<u64> {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with("mimic_mesh_inbound_refusals_total"))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
    }
}

fn meshed(pod: Option<&str>) -> MeshState {
    MeshState::new(&MeshConfig {
        enabled: true,
        pod: pod.map(str::to_string),
        ..MeshConfig::default()
    })
}

/// Reads to the end of the connection; a reset is an error, a FIN gives the bytes.
async fn read_all(c: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut out))
        .await
        .expect("the end of the connection in time")?;
    Ok(out)
}

fn want_head(dst: SocketAddr, origin: &str) -> String {
    format!(
        "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: connect-failed\r\nx-mimic-endpoint: {dst}\r\nx-mimic-origin: {origin}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

#[tokio::test]
async fn rty_27_a_closed_app_is_answered_503_connect_failed_on_http1() {
    let app = closed_port().await;
    let state = meshed(Some("cart-7d9f-x2k"));
    let rig = start(app, Some(&state)).await;
    let want = want_head(app, "inbound; pod=cart-7d9f-x2k");
    let body = vec![b'x'; 1024];
    let mut post = format!(
        "POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    post.extend_from_slice(&body);
    let get = b"GET /a HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n".to_vec();
    for (name, request, split) in [
        ("get", &get, None),
        ("post", &post, None),
        ("post split", &post, Some(post.len() - 1024)),
    ] {
        let mut c = TcpStream::connect(rig.addr).await.unwrap();
        match split {
            None => c.write_all(request).await.unwrap(),
            Some(at) => {
                c.write_all(&request[..at]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                c.write_all(&request[at..]).await.unwrap();
            }
        }
        let got = read_all(&mut c)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(String::from_utf8_lossy(&got), want, "{name}");
    }
    // Without a pod name the origin names the side only.
    let rig = start(app, Some(&meshed(None))).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(&get).await.unwrap();
    let got = read_all(&mut c).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&got), want_head(app, "inbound"));
}

#[tokio::test]
async fn rty_27_an_ipv6_pod_address_is_written_in_brackets() {
    let Ok(socket) = tokio::net::TcpSocket::new_v6() else {
        return;
    };
    if socket.bind("[::1]:0".parse().unwrap()).is_err() {
        return;
    }
    let app = socket.local_addr().unwrap();
    std::mem::forget(socket);
    let state = meshed(Some("p"));
    let rig = start(app, Some(&state)).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let got = read_all(&mut c).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got),
        want_head(app, "inbound; pod=p")
    );
}

async fn h2_streams(addr: SocketAddr, n: usize) -> Vec<(u16, Vec<(String, String)>, Vec<u8>)> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    let mut tasks = Vec::new();
    for _ in 0..n {
        let mut sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            let request = Request::get("http://cart.shop.svc/")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = sender.send_request(request).await.expect("an answer");
            let (head, body) = response.into_parts();
            let mut headers: Vec<(String, String)> = head
                .headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_string()))
                .collect();
            headers.sort();
            let body = body.collect().await.unwrap().to_bytes().to_vec();
            (head.status.as_u16(), headers, body)
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    // `GOAWAY`: the connection ends by itself.
    tokio::time::timeout(TIMEOUT, driver)
        .await
        .expect("the connection ends after GOAWAY")
        .unwrap()
        .ok();
    out
}

#[tokio::test]
async fn rty_27_a_closed_app_is_answered_503_connect_failed_on_h2c_then_goaway() {
    let app = closed_port().await;
    let state = meshed(Some("cart-1"));
    let rig = start(app, Some(&state)).await;
    let answers = h2_streams(rig.addr, 2).await;
    assert_eq!(answers.len(), 2);
    for (status, headers, body) in answers {
        assert_eq!(status, 503);
        assert!(body.is_empty());
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "content-length",
                "x-mimic-endpoint",
                "x-mimic-error",
                "x-mimic-origin"
            ],
            "{headers:?}"
        );
        let get = |n: &str| headers.iter().find(|(h, _)| h == n).unwrap().1.clone();
        assert_eq!(get("content-length"), "0");
        assert_eq!(get("x-mimic-error"), "connect-failed");
        assert_eq!(get("x-mimic-endpoint"), app.to_string());
        assert_eq!(get("x-mimic-origin"), "inbound; pod=cart-1");
    }
}

#[tokio::test]
async fn rty_27_when_the_app_is_back_the_next_connection_is_relayed() {
    let live = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = live.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = live.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                if s.read(&mut buf).await.is_ok() {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                }
            });
        }
    });
    let state = meshed(Some("p"));
    let rig = start(closed_port().await, Some(&state)).await;
    let get = b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n";
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(get).await.unwrap();
    assert!(read_all(&mut c).await.unwrap().starts_with(b"HTTP/1.1 503"));
    *rig.target.lock().unwrap() = up;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(get).await.unwrap();
    let mut got = vec![0u8; 40];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut got[..]))
        .await
        .expect("an answer in time")
        .unwrap();
    assert!(
        got.starts_with(b"HTTP/1.1 200 OK"),
        "{}",
        String::from_utf8_lossy(&got)
    );
}

#[tokio::test]
async fn rty_27_a_load_request_is_answered_the_same_way() {
    let app = closed_port().await;
    let state = meshed(Some("p"));
    let rig = start(app, Some(&state)).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\nmimic-load-run: t\r\n\r\n")
        .await
        .unwrap();
    let got = read_all(&mut c).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got),
        want_head(app, "inbound; pod=p")
    );
}

#[tokio::test]
async fn rty_28_without_the_mesh_the_connection_is_closed_without_a_byte() {
    let app = closed_port().await;
    let off = MeshState::new(&MeshConfig {
        enabled: true,
        pod: Some("p".into()),
        ..MeshConfig::default()
    });
    off.hold(common::mesh_pods::view_text(&[], 0, false), "\"1-a\"");
    for rig in [start(app, None).await, start(app, Some(&off)).await] {
        let mut c = TcpStream::connect(rig.addr).await.unwrap();
        c.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let got = read_all(&mut c).await.unwrap_or_default();
        assert!(got.is_empty(), "{}", String::from_utf8_lossy(&got));
        assert_eq!(rig.refusals(), None);
    }
}

#[tokio::test]
async fn rty_28_what_the_sidecar_cannot_speak_to_is_closed_without_a_byte() {
    let app = closed_port().await;
    let state = meshed(Some("p"));
    let rig = start(app, Some(&state)).await;
    let tls_hello = [0x16u8, 0x03, 0x01, 0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 0x03];
    for (name, bytes) in [
        ("tls", &tls_hello[..]),
        ("not http", &b"\x00\x01binary\r\n\r\n"[..]),
        ("redis", &b"*1\r\n$4\r\nPING\r\n"[..]),
    ] {
        let mut c = TcpStream::connect(rig.addr).await.unwrap();
        c.write_all(bytes).await.unwrap();
        let got = read_all(&mut c).await.unwrap_or_default();
        assert!(got.is_empty(), "{name}: {}", String::from_utf8_lossy(&got));
    }
    assert_eq!(rig.refusals(), None, "nothing was answered");
}

#[tokio::test]
async fn rty_29_each_refused_connection_is_counted_and_logged_once() {
    let logs = capture_logs();
    let app = closed_port().await;
    let state = meshed(Some("p"));
    let rig = start(app, Some(&state)).await;
    for _ in 0..3 {
        let mut c = TcpStream::connect(rig.addr).await.unwrap();
        c.write_all(b"GET /secret-path?token=abc HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer s3cr3t\r\n\r\n")
            .await
            .unwrap();
        let _ = read_all(&mut c).await;
    }
    let answers = h2_streams(rig.addr, 2).await;
    assert_eq!(answers.len(), 2);
    assert_eq!(rig.refusals(), Some(4));
    logs.settled().await;
    let warns: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| {
            v["fields"]["message"] == "inbound connect failed, answered 503 connect-failed"
        })
        .collect();
    assert_eq!(warns.len(), 4, "{}", logs.text());
    for line in &warns {
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["fields"]["dst"], app.to_string());
        assert!(line["fields"]["error"].is_string());
    }
    let text = logs.text();
    for leak in ["secret-path", "s3cr3t", "token=abc", "cart.shop.svc"] {
        assert!(!text.contains(leak), "request data in the logs");
    }
    assert!(
        state_label_free(&rig.metrics.encode()),
        "the counter has no label"
    );
}

fn state_label_free(scrape: &str) -> bool {
    scrape
        .lines()
        .filter(|l| l.starts_with("mimic_mesh_inbound_refusals_total"))
        .all(|l| l.starts_with("mimic_mesh_inbound_refusals_total "))
}

#[tokio::test]
async fn rty_28_an_app_that_forges_the_answer_is_stripped() {
    let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = app.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = app.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                if s.read(&mut buf).await.is_ok() {
                    let _ = s
                        .write_all(format!("HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: connect-failed\r\nx-mimic-endpoint: {addr}\r\nx-mimic-origin: inbound\r\nContent-Length: 0\r\n\r\n").as_bytes())
                        .await;
                }
            });
        }
    });
    let state = meshed(Some("p"));
    let rig = start(addr, Some(&state)).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut got = vec![0u8; "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".len()];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut got))
        .await
        .expect("an answer")
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
    );
    assert_eq!(rig.refusals(), None);
}
