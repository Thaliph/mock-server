//! MON-08 (active connections) and MON-09 (proxy overhead) of the dashboard metrics (spec §9,
//! #385), in process. Every wait has a deadline.

mod common;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{echo_server, fixture, fixture_configured, mock_rig};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A keep-alive HTTP/1.1 server: every request is answered `200` after `delay`.
async fn http_app(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut held = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    while let Some(end) = held.windows(4).position(|w| w == b"\r\n\r\n") {
                        held.drain(..end + 4);
                        tokio::time::sleep(delay).await;
                        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                        if s.write_all(ok).await.is_err() {
                            return;
                        }
                    }
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => held.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
            });
        }
    });
    addr
}

/// One request on `c`, read until the `ok` body.
async fn get(c: &mut TcpStream, path: &str) {
    c.write_all(format!("GET {path} HTTP/1.1\r\nHost: svc.test:8080\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while !got.ends_with(b"ok") {
        let n = tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(n > 0, "closed before the answer");
        got.extend_from_slice(&buf[..n]);
    }
}

/// The sum of the series whose line starts with `prefix`.
fn sum(metrics: &Metrics, prefix: &str) -> f64 {
    metrics
        .encode()
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn active(metrics: &Metrics, direction: &str) -> f64 {
    let line = format!("mimic_proxy_active_connections{{direction=\"{direction}\"}} ");
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(&line))
        .and_then(|l| l[line.len()..].parse::<f64>().ok())
        .unwrap_or(-1.0)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + TIMEOUT;
    while Instant::now() < end {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} not reached");
}

#[tokio::test]
async fn mon_08_active_connections_follow_the_open_connections_per_direction() {
    let app = http_app(Duration::ZERO).await;
    let echo = echo_server().await;
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel = TunnelMetrics::register(&mut metrics);
    let inbound = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let outbound = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tunnelled = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (in_addr, out_addr, tun_addr) = (
        inbound.local_addr().unwrap(),
        outbound.local_addr().unwrap(),
        tunnelled.local_addr().unwrap(),
    );
    let original: OriginalDst = Arc::new(move |s| {
        Ok(if s.local_addr()?.port() == tun_addr.port() {
            echo
        } else {
            app
        })
    });
    let shared = Arc::new(Shared::new(
        vec![in_addr, out_addr, tun_addr],
        tunnel,
        original,
    ));
    spawn_listener(inbound, Direction::Inbound, shared.clone());
    spawn_listener(tunnelled, Direction::Inbound, shared.clone());
    spawn_listener(outbound, Direction::Outbound, shared.clone());
    let metrics = Arc::new(metrics);
    // Both series exist from the start.
    assert_eq!(active(&metrics, "inbound"), 0.0);
    assert_eq!(active(&metrics, "outbound"), 0.0);

    let mut open = Vec::new();
    for _ in 0..2 {
        let mut c = TcpStream::connect(in_addr).await.unwrap();
        get(&mut c, "/a").await;
        open.push(c);
    }
    // An opaque TCP tunnel counts too.
    let mut t = TcpStream::connect(tun_addr).await.unwrap();
    t.write_all(b"ping\n").await.unwrap();
    let mut back = [0u8; 5];
    t.read_exact(&mut back).await.unwrap();
    open.push(t);
    for _ in 0..2 {
        let mut c = TcpStream::connect(out_addr).await.unwrap();
        get(&mut c, "/b").await;
        open.push(c);
    }
    until("3 and 2", || {
        active(&metrics, "inbound") == 3.0 && active(&metrics, "outbound") == 2.0
    })
    .await;
    drop(open);
    // Within 1 s of the close.
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end
        && (active(&metrics, "inbound") != 0.0 || active(&metrics, "outbound") != 0.0)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(active(&metrics, "inbound"), 0.0);
    assert_eq!(active(&metrics, "outbound"), 0.0);
}

#[tokio::test]
async fn mon_08_active_connections_never_exceed_the_cap() {
    let echo = echo_server().await;
    let f = fixture_configured(
        Direction::Inbound,
        move |_| Some(echo),
        |shared| shared.with_max_connections(2),
    )
    .await;
    let mut open = Vec::new();
    for _ in 0..4 {
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(b"ping\n").await.unwrap();
        open.push(c);
    }
    until("2 active", || active(&f.metrics, "inbound") == 2.0).await;
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        assert!(active(&f.metrics, "inbound") <= 2.0);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The observations of the overhead of `direction`, and those in the `le="0.1"` bucket.
fn overhead(metrics: &Metrics, direction: &str) -> (f64, f64) {
    let count = sum(
        metrics,
        &format!("mimic_proxy_overhead_seconds_count{{direction=\"{direction}\"}}"),
    );
    let le = sum(
        metrics,
        &format!("mimic_proxy_overhead_seconds_bucket{{direction=\"{direction}\",le=\"0.1\"}}"),
    );
    (count, le)
}

#[tokio::test]
async fn mon_09_http1_overhead_excludes_the_upstream_wait() {
    for direction in [Direction::Inbound, Direction::Outbound] {
        let d = direction.as_str();
        let app = http_app(Duration::from_millis(200)).await;
        let f = fixture(direction, move |_| Some(app)).await;
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        for n in 0..20 {
            get(&mut c, &format!("/p/{n}")).await;
        }
        until("20 observations", || overhead(&f.metrics, d).0 == 20.0).await;
        assert_eq!(overhead(&f.metrics, d), (20.0, 20.0), "{direction:?}");
        // The upstream wait is in the request duration, not in the overhead.
        let total = sum(
            &f.metrics,
            &format!("mimic_http_request_duration_seconds_sum{{direction=\"{d}\""),
        );
        assert!(total >= 20.0 * 0.2, "{direction:?}: {total}");

        // A tunnel and an unframed exchange add no observation.
        let mut t = TcpStream::connect(f.addr).await.unwrap();
        t.write_all(&[0u8, 1, 2, 3, 4, 5]).await.unwrap();
        let mut u = TcpStream::connect(f.addr).await.unwrap();
        u.write_all(b"GET / HTTP/1.1\r\nHost: svc.test\r\nBad Header\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(overhead(&f.metrics, d).0, 20.0, "{direction:?}");
    }
}

type Body = BoxBody<Bytes, io::Error>;

#[tokio::test]
async fn mon_09_h2_overhead_excludes_the_upstream_wait() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(|_: Request<Incoming>| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let body: Body = Full::new(Bytes::from_static(b"ok"))
                        .map_err(|never| match never {})
                        .boxed();
                    Ok::<_, io::Error>(Response::new(body))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    let f = fixture(Direction::Inbound, move |_| Some(app)).await;
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut calls = Vec::new();
    for n in 0..20 {
        let mut sender = sender.clone();
        calls.push(tokio::spawn(async move {
            let req = Request::get(format!("http://svc.test/s/{n}"))
                .body(
                    Full::new(Bytes::new())
                        .map_err(|never| match never {})
                        .boxed(),
                )
                .unwrap();
            let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
                .await
                .expect("answered")
                .unwrap();
            response.into_body().collect().await.unwrap();
        }));
    }
    for call in calls {
        call.await.unwrap();
    }
    until("20 observations", || {
        overhead(&f.metrics, "inbound").0 == 20.0
    })
    .await;
    assert_eq!(overhead(&f.metrics, "inbound"), (20.0, 20.0));
    let total = sum(
        &f.metrics,
        "mimic_http_request_duration_seconds_sum{direction=\"inbound\"",
    );
    assert!(total >= 20.0 * 0.2, "{total}");
}

#[tokio::test]
async fn mon_09_mocked_exchanges_exclude_the_simulated_latency() {
    // Recorded latency 200 ms, factor 1.
    let rig = mock_rig::rig(mock_rig::users_set(200), 100, 64).await;
    let mut c = TcpStream::connect(rig.addr).await.unwrap();
    for n in 1..=5 {
        c.write_all(
            format!(
                "GET /users/{n} HTTP/1.1\r\nHost: {}\r\n\r\n",
                mock_rig::AUTHORITY
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        let begun = Instant::now();
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let k = tokio::time::timeout(TIMEOUT, c.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(k > 0);
            got.extend_from_slice(&buf[..k]);
        }
        assert!(
            begun.elapsed() >= Duration::from_millis(150),
            "the mock latency applies"
        );
        // Let the small body arrive before the next request.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    until("5 observations", || {
        overhead(&rig.metrics, "outbound").0 == 5.0
    })
    .await;
    assert_eq!(overhead(&rig.metrics, "outbound"), (5.0, 5.0));
}
