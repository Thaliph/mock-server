//! RTY-19: three tiers in process. A's client calls B through A's sidecar, B's app forwards to C
//! through B's sidecar, and the three C pods all refuse. A trusted refusal is consumed by the
//! hop that receives it (spec §7.9 "Retries"): it never crosses a tier.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use common::mesh_pods::{H1Client, cluster_ip, view_on};
use common::{capture_logs, fixture, fixture_mesh};
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use mimic_sidecar::red::ProxyMetrics;
use tokio::net::TcpListener;

/// A pod of C: `503 overloaded` naming itself, counting its requests.
async fn c_pod() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                async move {
                    let _ = req.into_body().collect().await;
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(503)
                            .header("x-mimic-error", "overloaded")
                            .header("x-mimic-endpoint", addr.to_string())
                            .body(Full::new(Bytes::from("busy")))
                            .unwrap(),
                    )
                }
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, count)
}

/// B's app: forwards every request through `outbound` and passes on the status, the fields and
/// the body of the answer.
async fn b_app(outbound: SocketAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let (parts, body) = req.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    let socket = tokio::net::TcpStream::connect(outbound).await.unwrap();
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake(TokioIo::new(socket))
                            .await
                            .unwrap();
                    tokio::spawn(conn);
                    let mut forward = Request::builder()
                        .method(parts.method)
                        .uri(parts.uri)
                        .header("host", "cart.shop.svc");
                    if let Some(length) = parts.headers.get("content-length") {
                        forward = forward.header("content-length", length);
                    }
                    let answer = sender
                        .send_request(forward.body(Full::new(body)).unwrap())
                        .await
                        .unwrap();
                    let (head, body) = answer.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    let mut out = Response::builder().status(head.status);
                    for (name, value) in &head.headers {
                        if name != "content-length" {
                            out = out.header(name, value);
                        }
                    }
                    Ok::<_, Infallible>(out.body(Full::new(body)).unwrap())
                }
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, count)
}

/// B's inbound sidecar in front of `app`; `mesh` is `None` for `MIMIC_MESH=off`.
async fn inbound(app: SocketAddr, mesh: Option<&MeshState>) -> SocketAddr {
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
    // The metrics live as long as the listener's tasks.
    std::mem::forget(metrics);
    addr
}

fn mesh_state(cluster: SocketAddr, pods: &[SocketAddr]) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(3),
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

fn metric_sum(scrape: &str, name: &str) -> f64 {
    scrape
        .lines()
        .filter(|l| l.starts_with(name))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// 20 requests of A's client, B's sidecar meshed or not.
async fn three_tiers(b_meshed: bool) {
    const REQUESTS: usize = 20;
    let logs = capture_logs();
    let mut cs = Vec::new();
    for _ in 0..3 {
        cs.push(c_pod().await);
    }
    let c_addrs: Vec<SocketAddr> = cs.iter().map(|c| c.0).collect();
    let c_total = || cs.iter().map(|c| c.1.load(Ordering::SeqCst)).sum::<usize>();

    // B's outbound side: meshed it balances over the three pods; with the mesh off it is the
    // plain proxy to the original destination, one pod of C.
    let b_cluster: SocketAddr = "127.0.0.1:81".parse().unwrap();
    let b_state = mesh_state(b_cluster, &c_addrs);
    let b_out = if b_meshed {
        fixture_mesh(b_cluster, &b_state, None).await.addr
    } else {
        let one = c_addrs[0];
        fixture(Direction::Outbound, move |_| Some(one)).await.addr
    };
    let (app, app_count) = b_app(b_out).await;
    let b_in_state = MeshState::new(&MeshConfig {
        enabled: true,
        ..MeshConfig::default()
    });
    let b_in = inbound(app, b_meshed.then_some(&b_in_state)).await;

    // A's outbound side: B is the only pod of its Service.
    let a_state = mesh_state(cluster_ip(), &[b_in]);
    let a = fixture_mesh(cluster_ip(), &a_state, None).await;
    let mut client = H1Client::connect(a.addr).await;

    for n in 1..=REQUESTS {
        let before = c_total();
        let body = vec![b'x'; 100];
        let mut request =
            b"POST /p HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 100\r\n\r\n".to_vec();
        request.extend_from_slice(&body);
        client.send(&request).await;
        let (head, got) = client.read().await.expect("an answer");
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
        assert_eq!(got, b"busy");
        let lower = head.to_lowercase();
        if b_meshed {
            assert!(!lower.contains("x-mimic-"), "{head}");
        } else {
            let named = lower
                .lines()
                .find_map(|l| l.strip_prefix("x-mimic-endpoint: "))
                .map(str::to_string);
            assert!(
                named.is_some_and(|e| c_addrs.iter().any(|c| c.to_string() == e)),
                "{head}"
            );
        }
        // B's app was reached once, and C at most 4 times (once when B's sidecar is off).
        assert_eq!(app_count.load(Ordering::SeqCst), n);
        let tries = c_total() - before;
        if b_meshed {
            assert!((2..=4).contains(&tries), "{tries} tries");
        } else {
            assert_eq!(tries, 1);
        }
    }

    // A's sidecar retried nothing and saw one try per request.
    let scrape = a.metrics.encode();
    assert_eq!(
        metric_sum(&scrape, "mimic_mesh_retries_total{"),
        0.0,
        "{scrape}"
    );
    logs.settled().await;
    let b_in = b_in.to_string();
    let lines: Vec<_> = logs
        .access()
        .into_iter()
        .filter(|l| l["upstream"].as_str() == Some(b_in.as_str()))
        .collect();
    assert_eq!(lines.len(), REQUESTS, "{lines:?}");
    assert!(lines.iter().all(|l| l["attempts"].as_u64() == Some(1)));
}

#[tokio::test]
async fn rty_19_a_refusal_is_consumed_by_the_hop_that_receives_it() {
    three_tiers(true).await;
}

#[tokio::test]
async fn rty_19_with_b_unmeshed_the_binding_keeps_a_from_retrying() {
    three_tiers(false).await;
}
