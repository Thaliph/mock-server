//! RTY-18: the inbound side of a meshed sidecar removes the `x-mimic-*` fields of its app's
//! final responses (spec §4.1 "Mimic fields of app responses"); with the mesh off, or a view of
//! `"mesh": false`, the relay is byte-exact. An inbound sidecar in process and a fake app.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::mesh_pods::view_text;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::red::ProxyMetrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

const GET: &[u8] = b"GET /a HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n";
const HEAD: &[u8] = b"HEAD /a HTTP/1.1\r\nHost: cart.shop.svc\r\n\r\n";

/// An app that answers every request head with `reply`, then closes when `close`.
async fn app(reply: &'static [u8], close: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut pending = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    while let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                        pending.drain(..p + 4);
                        if s.write_all(reply).await.is_err() {
                            return;
                        }
                        if close {
                            let _ = s.shutdown().await;
                            return;
                        }
                    }
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
            });
        }
    });
    addr
}

/// An `h2c` app answering `200` with the given response headers.
async fn app_h2(headers: &'static [(&'static str, &'static str)]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = service_fn(move |_: Request<hyper::body::Incoming>| async move {
                    let mut response = Response::builder().status(200);
                    for (n, v) in headers {
                        response = response.header(*n, *v);
                    }
                    Ok::<_, Infallible>(response.body(Full::new(Bytes::from("ok"))).unwrap())
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    addr
}

struct Rig {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
}

/// An inbound listener in front of `app`; the mesh is `Some(state)` or off.
async fn start(app: SocketAddr, mesh: Option<&MeshState>) -> Rig {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel = TunnelMetrics::register(&mut metrics);
    let original: OriginalDst = Arc::new(move |_| Ok(app));
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
    }
}

impl Rig {
    /// The counter, 0 until the first field was removed.
    fn stripped(&self) -> u64 {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with("mimic_mesh_stripped_headers_total"))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
}

fn meshed() -> MeshState {
    MeshState::new(&MeshConfig {
        enabled: true,
        ..MeshConfig::default()
    })
}

/// Sends `request` `times` times on one connection and reads `want.len()` bytes after each
/// (then to the end of the connection when `to_end`).
async fn exchange(addr: SocketAddr, request: &[u8], times: usize, want: &[u8], to_end: bool) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    for _ in 0..times {
        c.write_all(request).await.unwrap();
        let mut got = vec![0u8; want.len()];
        tokio::time::timeout(TIMEOUT, c.read_exact(&mut got))
            .await
            .expect("an answer in time")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(want));
    }
    if to_end {
        let mut rest = Vec::new();
        tokio::time::timeout(TIMEOUT, c.read_to_end(&mut rest))
            .await
            .expect("the close in time")
            .unwrap();
        assert!(rest.is_empty(), "nothing after the answer");
    }
}

#[tokio::test]
async fn rty_18_http1_content_length_answers_lose_the_fields_on_a_keep_alive_connection() {
    let reply: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nContent-Type: text/plain\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-other: 1\r\nContent-Length: 2\r\n\r\nok";
    let want: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 3, want, false).await;
    assert_eq!(rig.stripped(), 9, "3 per response");
}

#[tokio::test]
async fn rty_18_http1_chunked_and_close_delimited_bodies_are_untouched() {
    let chunked: &'static [u8] = b"HTTP/1.1 200 OK\r\nx-mimic-error: overloaded\r\nTransfer-Encoding: chunked\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-other: 1\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let want: &[u8] =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let state = meshed();
    let rig = start(app(chunked, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 2, want, false).await;
    assert_eq!(rig.stripped(), 6);

    let delimited: &'static [u8] = b"HTTP/1.1 200 OK\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-error: overloaded\r\nx-mimic-other: 1\r\nConnection: close\r\n\r\nuntil the close";
    let want: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil the close";
    let state = meshed();
    let rig = start(app(delimited, true).await, Some(&state)).await;
    exchange(rig.addr, GET, 1, want, true).await;
    assert_eq!(rig.stripped(), 3);
}

#[tokio::test]
async fn rty_18_http1_a_head_answer_loses_the_fields() {
    let reply: &'static [u8] = b"HTTP/1.1 200 OK\r\nx-mimic-error: overloaded\r\nContent-Length: 10\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-other: 1\r\n\r\n";
    let want: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, HEAD, 2, want, false).await;
    assert_eq!(rig.stripped(), 6);
}

#[tokio::test]
async fn rty_18_http1_an_answer_without_such_a_field_is_byte_exact_and_counts_nothing() {
    let reply: &'static [u8] = b"HTTP/1.1 200 OK\r\ncontent-TYPE: a\r\nX-Mimicry: 1\r\nSet-Cookie: a=1\r\nset-cookie: b=2\r\nContent-Length: 2\r\n\r\nok";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 2, reply, false).await;
    assert_eq!(rig.stripped(), 0);
}

#[tokio::test]
async fn rty_18_http1_a_100_continue_is_relayed_unchanged_and_the_final_head_is_stripped() {
    let reply: &'static [u8] = b"HTTP/1.1 100 Continue\r\nx-mimic-interim: 1\r\n\r\nHTTP/1.1 200 OK\r\nx-mimic-error: overloaded\r\nContent-Length: 2\r\n\r\nok";
    let want: &[u8] = b"HTTP/1.1 100 Continue\r\nx-mimic-interim: 1\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 1, want, false).await;
    assert_eq!(rig.stripped(), 1);
}

#[tokio::test]
async fn rty_18_http1_the_head_arriving_in_pieces_is_judged_whole() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        for piece in [
            &b"HTTP/1.1 200 OK\r\nx-mimi"[..],
            b"c-error: over",
            b"loaded\r\nContent-Length: 2\r\n",
            b"\r\nok",
        ] {
            s.write_all(piece).await.unwrap();
            s.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let state = meshed();
    let rig = start(addr, Some(&state)).await;
    exchange(
        rig.addr,
        GET,
        1,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        false,
    )
    .await;
    assert_eq!(rig.stripped(), 1);
}

#[tokio::test]
async fn rty_18_with_the_mesh_off_the_fields_stay_byte_for_byte() {
    let reply: &'static [u8] = b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nContent-Type: text/plain\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-other: 1\r\nContent-Length: 2\r\n\r\nok";
    let rig = start(app(reply, false).await, None).await;
    exchange(rig.addr, GET, 2, reply, false).await;
}

#[tokio::test]
async fn rty_18_with_a_view_of_mesh_false_the_fields_stay_and_with_the_view_back_on_they_go() {
    let reply: &'static [u8] = b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nContent-Type: text/plain\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nx-mimic-other: 1\r\nContent-Length: 2\r\n\r\nok";
    let want: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";
    let state = meshed();
    let pod: SocketAddr = "127.0.0.1:9".parse().unwrap();
    state.hold(view_text(&[pod], 0, false), "\"1-a\"");
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 1, reply, false).await;
    assert_eq!(rig.stripped(), 0);
    state.hold(view_text(&[pod], 0, true), "\"2-a\"");
    exchange(rig.addr, GET, 1, want, false).await;
    assert_eq!(rig.stripped(), 3);
}

#[tokio::test]
async fn rty_18_h2c_the_fields_are_gone_and_the_others_kept() {
    const HEADERS: &[(&str, &str)] = &[
        ("x-mimic-error", "overloaded"),
        ("x-mimic-endpoint", "10.0.0.9:80"),
        ("x-mimic-other", "1"),
        ("x-keep", "1"),
        ("content-type", "text/plain"),
    ];
    let state = meshed();
    let on = start(app_h2(HEADERS).await, Some(&state)).await;
    let off = start(app_h2(HEADERS).await, None).await;
    for (rig, stripped) in [(&on, true), (&off, false)] {
        let stream = TcpStream::connect(rig.addr).await.unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(connection);
        for _ in 0..2 {
            let request = Request::get("http://cart.shop.svc/")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = tokio::time::timeout(TIMEOUT, sender.send_request(request))
                .await
                .expect("an answer in time")
                .unwrap();
            let names: Vec<&str> = response.headers().keys().map(|n| n.as_str()).collect();
            let mimic = names.iter().filter(|n| n.starts_with("x-mimic-")).count();
            assert_eq!(mimic, if stripped { 0 } else { 3 }, "{names:?}");
            assert_eq!(response.headers()["x-keep"], "1");
            assert_eq!(response.headers()["content-type"], "text/plain");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], b"ok");
        }
    }
    assert_eq!(on.stripped(), 6, "3 per response");
}

/// A raw app: answers each request head with `reply` built at run time.
async fn app_with(reply: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {
                    if s.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn rty_18_blank_lines_before_the_status_line_do_not_hide_the_fields_from_a_large_body() {
    // The codec skips the blank lines and frames the head; a 64 KiB body behind it must not
    // make the relay give up holding the head.
    let body = vec![b'a'; 70_000];
    for lead in [&b"\r\n"[..], b"\n", b"\r\n\r\n\n"] {
        let mut reply = lead.to_vec();
        reply.extend_from_slice(
            b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nX-Mimic-Endpoint: 10.0.0.9:80\r\nContent-Length: 70000\r\n\r\n",
        );
        reply.extend_from_slice(&body);
        let mut want = lead.to_vec();
        want.extend_from_slice(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 70000\r\n\r\n",
        );
        want.extend_from_slice(&body);
        let state = meshed();
        let rig = start(app_with(reply).await, Some(&state)).await;
        exchange(rig.addr, GET, 2, &want, false).await;
        assert_eq!(rig.stripped(), 4);
    }
}

#[tokio::test]
async fn rty_18_a_reason_phrase_that_is_not_utf8_does_not_hide_the_fields() {
    let reply: &'static [u8] =
        b"HTTP/1.1 503 Unavail\xffable\r\nx-mimic-error: overloaded\r\nContent-Length: 2\r\n\r\nok";
    let want: &[u8] = b"HTTP/1.1 503 Unavail\xffable\r\nContent-Length: 2\r\n\r\nok";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 2, want, false).await;
    assert_eq!(rig.stripped(), 2);
}

#[tokio::test]
async fn rty_18_a_head_the_codec_cannot_frame_is_relayed_at_once_unchanged() {
    // Spec §4.1 and §11.1: not held until 64 KiB.
    let reply: &'static [u8] = b"garbage\r\nx-mimic-error: overloaded\r\n\r\n";
    let state = meshed();
    let rig = start(app(reply, false).await, Some(&state)).await;
    exchange(rig.addr, GET, 1, reply, false).await;
}

#[tokio::test]
async fn rty_18_endless_blank_lines_before_the_status_line_close_the_connection_unrelayed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        let blanks = vec![b'\n'; 70_000];
        let _ = s.write_all(&blanks).await;
        let _ = s
            .write_all(b"HTTP/1.1 503 X\r\nx-mimic-error: overloaded\r\n\r\n")
            .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let state = meshed();
    let rig = start(addr, Some(&state)).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    c.write_all(GET).await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut got))
        .await
        .expect("the close in time")
        .ok();
    assert!(
        !got.windows(7).any(|w| w == b"x-mimic"),
        "no field reaches the client"
    );
}
