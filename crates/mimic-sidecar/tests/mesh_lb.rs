//! MSH-48 to MSH-54: the sidecar balances in-scope cleartext HTTP over the pods of the view
//! (spec §7.9 "Load balancing", §4.1 "Mesh-routed HTTP"). The pods are local listeners.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::mesh_pods::{H1Client, Pod, RawPod, cluster_ip, h2c_calls, view, view_text};
use common::{capture_logs, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A state that holds `pods` as the Service `shop/cart`, with a fixed seed for the draws.
fn state_with(pods: &[SocketAddr], configure: impl FnOnce(&mut MeshConfig)) -> MeshState {
    let mut config = MeshConfig {
        enabled: true,
        seed: Some(7),
        ..MeshConfig::default()
    };
    configure(&mut config);
    let state = MeshState::new(&config);
    state.hold(view(pods, 0), "\"1-a\"");
    state
}

fn spread(bodies: impl IntoIterator<Item = String>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for body in bodies {
        *counts.entry(body).or_insert(0) += 1;
    }
    counts
}

async fn three() -> (Pod, Pod, Pod) {
    (
        Pod::http1("a", 0).await,
        Pod::http1("b", 0).await,
        Pod::http1("c", 0).await,
    )
}

async fn settle<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what}");
}

#[tokio::test]
async fn msh_48_one_http1_keepalive_client_is_spread_over_the_three_pods() {
    let _logs = capture_logs();
    let (a, b, c) = three().await;
    let state = state_with(&[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut bodies = Vec::new();
    for n in 0..3000 {
        let (head, body) = client.get(&format!("/{n}")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        bodies.push(body);
    }
    let counts = spread(bodies);
    for (pod, n) in &counts {
        assert!((900..=1100).contains(n), "{pod} got {n}: {counts:?}");
    }
    assert_eq!(counts.len(), 3, "{counts:?}");
    for pod in [&a, &b, &c] {
        let metric = format!(
            "mimic_mesh_endpoint_requests_total{{backend=\"shop/cart:80\",endpoint=\"{}\",status_class=\"2xx\"}} {}",
            pod.addr,
            pod.counts.requests()
        );
        assert!(
            f.metrics.encode().contains(&metric),
            "{metric}\n{}",
            f.metrics.encode()
        );
    }
}

#[tokio::test]
async fn msh_48_one_h2c_client_is_spread_over_the_three_pods() {
    let _logs = capture_logs();
    let a = Pod::h2c("a", 0, None).await;
    let b = Pod::h2c("b", 0, None).await;
    let c = Pod::h2c("c", 0, None).await;
    let state = state_with(&[a.addr, b.addr, c.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    // Each request takes a moment so that 8 are in flight at once.
    for p in [&a, &b, &c] {
        p.counts.delay_ms.store(2, Ordering::SeqCst);
    }
    let counts = spread(h2c_calls(f.addr, 3000, 8).await);
    for (pod, n) in &counts {
        assert!((900..=1100).contains(n), "{pod} got {n}: {counts:?}");
    }
    assert_eq!(counts.len(), 3, "{counts:?}");
}

#[tokio::test]
async fn msh_49_a_slow_pod_gets_fewer_requests() {
    let _logs = capture_logs();
    let a = Pod::http1("a", 0).await;
    let b = Pod::http1("b", 0).await;
    let slow = Pod::http1("slow", 50).await;
    let state = state_with(&[a.addr, b.addr, slow.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let addr = f.addr;
    let mut clients = Vec::new();
    for _ in 0..8 {
        clients.push(tokio::spawn(async move {
            let mut client = H1Client::connect(addr).await;
            for _ in 0..75 {
                let (head, _) = client.get("/").await;
                assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            }
        }));
    }
    for c in clients {
        c.await.unwrap();
    }
    let (fast, slow) = (
        a.counts.requests().min(b.counts.requests()),
        slow.counts.requests(),
    );
    assert_eq!(a.counts.requests() + b.counts.requests() + slow, 600);
    assert!(
        slow * 10 <= fast * 6,
        "the slow pod got {slow}, the fast ones {} and {}",
        a.counts.requests(),
        b.counts.requests()
    );
}

#[tokio::test]
async fn msh_49_a_new_pod_ramps_and_a_removed_pod_drains() {
    let _logs = capture_logs();
    let a = Pod::http1("a", 5).await;
    let b = Pod::http1("b", 5).await;
    let c = Pod::http1("c", 5).await;
    let state = state_with(&[a.addr, b.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let addr = f.addr;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut clients = Vec::new();
    for _ in 0..8 {
        let stop = stop.clone();
        clients.push(tokio::spawn(async move {
            let mut client = H1Client::connect(addr).await;
            while !stop.load(Ordering::SeqCst) {
                let (head, _) = client.get("/").await;
                assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // `c` joins with a 1 s slow start.
    let before = a.counts.requests() + b.counts.requests();
    state.hold(view(&[a.addr, b.addr, c.addr], 1), "\"1-b\"");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let early = c.counts.requests();
    let total = a.counts.requests() + b.counts.requests() + early - before;
    assert!(
        early * 5 <= total,
        "c got {early} of the first {total} requests"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let mid = c.counts.requests();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        c.counts.requests() >= mid + 20,
        "c got {} requests after its window",
        c.counts.requests() - mid
    );

    // `b` leaves: no new request, the ones in flight finish, its connections close.
    b.counts.delay_ms.store(300, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(350)).await;
    state.hold(view(&[a.addr, c.addr], 1), "\"1-c\"");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let gone = b.counts.requests();
    stop.store(true, Ordering::SeqCst);
    for client in clients {
        client.await.unwrap(); // every request of every client was answered `200`
    }
    assert_eq!(
        b.counts.requests(),
        gone,
        "b got requests after it left the view"
    );
    settle("b's connections close", || b.counts.open() == 0).await;
}

#[tokio::test]
async fn msh_50_http1_pools_reuse_connections_and_close_idle_ones() {
    let _logs = capture_logs();
    let pod = Pod::http1("a", 0).await;
    let state = state_with(&[pod.addr], |c| {
        c.pool_idle_timeout = Duration::from_millis(300)
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        client.get("/").await;
    }
    assert_eq!(
        pod.counts.accepted(),
        1,
        "ten sequential requests, one connection"
    );
    let mut other = H1Client::connect(f.addr).await;
    other.get("/").await;
    assert_eq!(
        pod.counts.accepted(),
        1,
        "another client reuses the pooled connection"
    );

    pod.counts.delay_ms.store(150, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let addr = f.addr;
        tasks.push(tokio::spawn(async move {
            H1Client::connect(addr).await.get("/").await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(pod.counts.accepted(), 5, "five at once, five connections");
    settle("idle connections are closed", || pod.counts.open() == 0).await;
}

#[tokio::test]
async fn msh_50_h2_streams_share_a_connection_and_a_second_one_opens_at_the_limit() {
    let _logs = capture_logs();
    let pod = Pod::h2c("a", 200, Some(2)).await;
    let state = state_with(&[pod.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let calls = tokio::spawn(h2c_calls_staggered(f.addr, 5));
    let bodies = calls.await.unwrap();
    assert_eq!(bodies, vec!["a"; 5]);
    let connections = pod.counts.accepted();
    assert!((2..=3).contains(&connections), "{connections} connections");
}

async fn h2c_calls_staggered(addr: SocketAddr, n: usize) -> Vec<String> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Full<Bytes>>(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut tasks = Vec::new();
    for _ in 0..n {
        let mut sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            let request = hyper::Request::get("http://cart.shop.svc/")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = sender.send_request(request).await.expect("an answer");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            String::from_utf8_lossy(&body).into_owned()
        }));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

/// What a canned pod answers: 200, `content-length` 2, closing nothing.
fn canned(_head: &[u8]) -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nX-Reply: Mixed-Case\r\nContent-Length: 2\r\n\r\nok".to_vec()
}

async fn roundtrip(addr: SocketAddr, bytes: &[u8], read: usize) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(bytes).await.unwrap();
    let mut got = vec![0u8; read];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut got))
        .await
        .expect("answered in time")
        .unwrap();
    got
}

/// The corpus of MSH-51: odd header case and order, a pipelined pair, `Connection: close`.
const CORPUS: &[u8] = b"GET /a HTTP/1.1\r\nhOsT: cart.shop.svc\r\nZ-Last: 1\r\nA-First: 2\r\nx-dup: 1\r\nX-Dup: 2\r\n\r\nGET /b HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn msh_51_out_of_scope_the_kill_switches_and_a_mesh_off_view_keep_the_byte_relay() {
    let _logs = capture_logs();
    let reply_len = canned(b"").len();
    // Today: no mesh at all.
    let pod = RawPod::start(canned).await;
    let plain = common::fixture_configured(
        mimic_sidecar::listener::Direction::Outbound,
        move |_| Some(pod.addr),
        |s| s,
    )
    .await;
    let reference = roundtrip(plain.addr, CORPUS, reply_len * 2).await;
    let expected_received = CORPUS.to_vec();

    // Out of scope: the view holds the Service, the destination is another address.
    let (scoped, other) = (Pod::http1("a", 0).await, RawPod::start(canned).await);
    let state = state_with(&[scoped.addr], |_| {});
    let f = fixture_mesh(other.addr, &state, None).await;
    assert_eq!(roundtrip(f.addr, CORPUS, reply_len * 2).await, reference);
    assert_eq!(other.received(), expected_received);
    assert_eq!(state.unscoped_connections(), 1);
    assert_eq!(scoped.counts.requests(), 0);

    // A view that switches the mesh off for the workload.
    let off_pod = RawPod::start(canned).await;
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        ..MeshConfig::default()
    });
    state.hold(view_text(&[scoped.addr], 0, false), "\"1-a\"");
    let f = fixture_mesh(off_pod.addr, &state, None).await;
    assert_eq!(roundtrip(f.addr, CORPUS, reply_len * 2).await, reference);
    assert_eq!(off_pod.received(), expected_received);

    // `MIMIC_MESH=off`: no state at all; a destination in the table is relayed too.
    let metrics = f.metrics.encode();
    let series: Vec<&str> = metrics
        .lines()
        .filter(|l| l.starts_with("mimic_mesh_") && !l.starts_with("mimic_mesh_view_"))
        .filter(|l| !l.starts_with("mimic_mesh_unscoped"))
        .collect();
    assert!(series.is_empty(), "{series:?}");
}

#[tokio::test]
async fn msh_52_discovery_records_one_exchange_under_the_apps_authority() {
    let logs = capture_logs();
    let pod = RawPod::start(canned).await;
    let state = state_with(&[pod.addr], |_| {});
    let discovery = mimic_sidecar::discovery::Discovery::start(
        &mimic_sidecar::discovery::DiscoveryConfig::default(),
        Some("pod-a".into()),
    )
    .unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let request = b"GET /items/42 HTTP/1.1\r\nHost: cart.shop.svc:80\r\nX-Mixed-Case: yes\r\n\r\n";
    let mut client = H1Client::connect(f.addr).await;
    client.send(request).await;
    let (head, body) = client.read().await.unwrap();
    // The bytes the app sees are the pod's, header case included.
    assert_eq!(
        head,
        "HTTP/1.1 200 OK\r\nX-Reply: Mixed-Case\r\nContent-Length: 2"
    );
    assert_eq!(body, b"ok");
    // And the pod got the request as sent, header case kept.
    let got = String::from_utf8(pod.received()).unwrap();
    assert!(got.starts_with("GET /items/42 HTTP/1.1\r\n"), "{got}");
    assert!(got.contains("\r\nX-Mixed-Case: yes\r\n"), "{got}");
    assert!(got.contains("\r\nHost: cart.shop.svc:80\r\n"), "{got}");

    common::counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 1",
    )
    .await;
    let report = discovery.report();
    let endpoints = report.endpoints();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].authority, Some("cart.shop.svc:80"));
    assert_eq!(endpoints[0].template, "/items/{items_id}");

    let lines = logs.access_exactly(1).await;
    let line = &lines[0];
    assert_eq!(line["dst"], "127.0.0.1:80", "the access line");
    assert_eq!(line["backend"], "shop/cart:80", "the access line");
    assert_eq!(line["upstream"], pod.addr.to_string(), "the access line");
    assert_eq!(line["authority"], "cart.shop.svc", "the access line");
    assert_eq!(line["status"], 200, "the access line");
}

#[tokio::test]
async fn msh_54_metrics_count_cap_and_evict() {
    let _logs = capture_logs();
    let (a, b, c) = three().await;
    let state = state_with(&[a.addr, b.addr, c.addr], |config| {
        config.max_endpoint_labels = 2;
    });
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for n in 0..30 {
        client.get(&format!("/{n}")).await;
    }
    let text = f.metrics.encode();
    let count = |prefix: &str| -> f64 {
        text.lines()
            .filter(|l| l.starts_with(prefix))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .sum()
    };
    assert!(
        text.contains("mimic_mesh_requests_total{service=\"shop/cart\",route=\"none\",backend=\"shop/cart:80\",status_class=\"2xx\",reason=\"none\"} 30"),
        "the metrics"
    );
    assert_eq!(
        count("mimic_mesh_request_duration_seconds_count"),
        30.0,
        "the metrics"
    );
    assert_eq!(
        count("mimic_mesh_endpoint_requests_total"),
        30.0,
        "the metrics"
    );
    let endpoints: std::collections::BTreeSet<&str> = text
        .lines()
        .filter(|l| l.starts_with("mimic_mesh_endpoint_requests_total{"))
        .filter_map(|l| l.split("endpoint=\"").nth(1)?.split('"').next())
        .collect();
    assert_eq!(endpoints.len(), 3, "two pods and `other`: {endpoints:?}");
    assert!(endpoints.contains("other"), "{endpoints:?}");
    assert!(
        text.contains(
            "mimic_mesh_endpoint_in_flight{backend=\"shop/cart:80\",endpoint=\"other\"} 0"
        ),
        "the metrics"
    );

    // A pod with a label of its own leaves the view: its series stay 4 minutes, and are gone
    // after 5; the slot is free for a pod added later.
    let labelled = endpoints
        .iter()
        .find(|e| **e != "other")
        .map(|e| e.to_string())
        .unwrap();
    let staying: Vec<SocketAddr> = [a.addr, b.addr, c.addr]
        .into_iter()
        .filter(|p| p.to_string() != labelled)
        .collect();
    state.hold(view(&staying, 0), "\"1-b\"");
    let now = Instant::now();
    let series = |at: Instant| {
        state.evict(at);
        f.metrics
            .encode()
            .lines()
            .filter(|l| l.contains(&format!("endpoint=\"{labelled}\"")))
            .count()
    };
    assert!(
        series(now + Duration::from_secs(240)) > 0,
        "kept for 4 minutes"
    );
    assert_eq!(series(now + Duration::from_secs(301)), 0, "gone after 5");
    let d = Pod::http1("d", 0).await;
    let mut pods = staying.clone();
    pods.push(d.addr);
    state.hold(view(&pods, 0), "\"1-c\"");
    for n in 0..90 {
        client.get(&format!("/{n}")).await;
    }
    assert!(
        f.metrics
            .encode()
            .contains(&format!("endpoint=\"{}\"", d.addr)),
        "{}",
        f.metrics.encode()
    );
}

const SERVICE_FAMILIES: [&str; 4] = [
    "mimic_mesh_requests_total{",
    "mimic_mesh_request_duration_seconds_count{",
    "mimic_mesh_fallbacks_total{",
    "mimic_mesh_reconnects_total{",
];

fn service_series(text: &str) -> usize {
    text.lines()
        .filter(|l| SERVICE_FAMILIES.iter().any(|f| l.starts_with(f)))
        .filter(|l| l.contains("backend=\"shop/cart:80\""))
        .count()
}

#[tokio::test]
async fn msh_79_the_series_of_a_service_that_left_the_view_are_evicted_after_5_minutes() {
    let _logs = capture_logs();
    let pod = Pod::http1("a", 0).await;
    let state = state_with(&[pod.addr], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client.get("/").await;
    assert_eq!(service_series(&f.metrics.encode()), 4, "{}", f.metrics.encode());

    // It leaves and comes back within the 5 minutes: nothing is removed.
    state.hold(view_text(&[], 0, false), "\"1-b\"");
    state.hold(view(&[pod.addr], 0), "\"1-c\"");
    let now = Instant::now();
    state.evict(now + Duration::from_secs(301));
    assert_eq!(service_series(&f.metrics.encode()), 4, "kept when back");

    // It leaves: kept 4 minutes, gone after 5.
    state.hold(view_text(&[], 0, false), "\"1-d\"");
    let now = Instant::now();
    state.evict(now + Duration::from_secs(240));
    assert_eq!(service_series(&f.metrics.encode()), 4, "kept for 4 minutes");
    state.evict(now + Duration::from_secs(301));
    assert_eq!(service_series(&f.metrics.encode()), 0, "gone after 5");
}
