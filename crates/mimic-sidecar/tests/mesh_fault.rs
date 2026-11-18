//! FLT-11 to FLT-14, FLT-21, FLT-24 and FLT-31: the aborts of the caller's sidecar (spec §7.9
//! "Fault injection", §12.28): the answer and its framing on HTTP/1, `h2c` and intercepted TLS,
//! the share drawn, what an abort does not take, the access line, which policy applies, and an
//! app answering `fault-injected` itself.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use common::mesh_pods::{H1Client, Pod, RawPod, Svc, cluster_ip, view_services};
use common::{capture_logs, fixture_mesh, h2_client, median, metric, until};
use http_body_util::{BodyExt, Empty};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The fault of FLT-11: a `POST` under `/checkout` is aborted `503`.
const CHECKOUT: &str = r#"[{"abort":{"percent":100,"status":503},"match":{"method":"POST","path":{"type":"PathPrefix","value":"/checkout"}}}]"#;

const POST_CHECKOUT: &[u8] =
    b"POST /checkout/1 HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 0\r\n\r\n";

fn state(services: &[Svc]) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(21),
        ..MeshConfig::default()
    });
    state.hold(view_services(services), "\"1-a\"");
    state
}

/// A fault matching `Exact path`, aborting `percent` % with `status`.
fn abort_on(path: &str, percent: u32, status: u16) -> String {
    format!(
        r#"{{"abort":{{"percent":{percent},"status":{status}}},"match":{{"path":{{"type":"Exact","value":"{path}"}}}}}}"#
    )
}

/// A backend of a rule.
fn backend(service: &str, weight: u32) -> String {
    format!(r#"{{"port":80,"service":"{service}","weight":{weight}}}"#)
}

/// The route `shop/r` with one rule per (match, backends, filters).
fn route(rules: &[(&str, &[String], &str)]) -> String {
    let rules: Vec<String> = rules
        .iter()
        .map(|(matches, backends, filters)| {
            let filters = if filters.is_empty() {
                String::new()
            } else {
                format!(r#","filters":{filters}"#)
            };
            format!(
                r#"{{"backends":[{}]{filters},"matches":[{matches}]}}"#,
                backends.join(",")
            )
        })
        .collect();
    format!(r#"{{"name":"shop/r","rules":[{}]}}"#, rules.join(","))
}

fn exact(path: &str) -> String {
    format!(r#"{{"path":{{"type":"Exact","value":"{path}"}}}}"#)
}

const EVERY_PATH: &str = r#"{"path":{"type":"PathPrefix","value":"/"}}"#;

fn lower(head: &str) -> String {
    head.to_ascii_lowercase()
}

fn status(head: &str) -> u16 {
    head[9..12].parse().unwrap()
}

/// The answer of an abort (spec §7.9 "Fault injection"): `status`, exactly one
/// `x-mimic-error: fault-injected`, `content-length: 0`, no pod named, no body, the connection
/// kept.
fn assert_aborted(head: &str, body: &[u8], want: u16) {
    assert_eq!(status(head), want, "{head}");
    let head = lower(head);
    assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
    assert!(head.contains("x-mimic-error: fault-injected"), "{head}");
    assert!(head.contains("content-length: 0"), "{head}");
    assert!(!head.contains("x-mimic-endpoint"), "{head}");
    assert!(!head.contains("connection:"), "kept: {head}");
    assert!(body.is_empty());
}

/// Whether the access line has no such field.
fn absent(line: &serde_json::Value, field: &str) -> bool {
    line.get(field).is_none_or(serde_json::Value::is_null)
}

fn no_series(scrape: &str, name: &str) -> bool {
    !scrape.lines().any(|l| l.starts_with(name))
}

/// Reads one head (and nothing after it) from `stream`.
async fn read_head(stream: &mut (impl AsyncReadExt + Unpin)) -> String {
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(TIMEOUT, stream.read(&mut byte))
            .await
            .expect("an answer in time")
            .expect("a read");
        assert!(n > 0, "closed before the answer");
        got.push(byte[0]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

#[tokio::test]
async fn flt_11_an_http1_abort_answers_the_status_and_keeps_the_connection() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let cluster = Pod::http1("cluster", 0).await;
    let cart =
        Svc::new("shop/cart", "127.0.0.1", cluster.addr.port(), &[pod.addr]).faults(CHECKOUT);
    let state = state(&[cart]);
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..20 {
        client.send(POST_CHECKOUT).await;
        let (head, body) = client.read().await.expect("an answer");
        assert_aborted(&head, &body, 503);
    }
    let (head, body) = client.get("/cart").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
    assert_eq!(pod.counts.requests(), 1, "only the GET reached a pod");
    assert_eq!(
        cluster.counts.accepted(),
        0,
        "nothing reached the ClusterIP"
    );
    let lines = logs.access_exactly(21).await;
    let aborted: Vec<u64> = lines
        .iter()
        .filter(|l| l["reason"] == "fault-injected")
        .map(|l| l["duration_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(aborted.len(), 20);
    assert!(median(aborted.clone()) <= 1, "{aborted:?}");
}

/// One `h2c` request: the status, the `x-mimic-error`, whether `x-mimic-endpoint` came, the
/// `content-length` and the body.
async fn h2_call(
    sender: &common::H2Sender,
    method: &str,
    path: &str,
) -> (u16, Option<String>, bool, Option<String>, Bytes) {
    let mut sender = sender.clone();
    let request = hyper::Request::builder()
        .method(method)
        .uri(format!("http://cart.shop.svc{path}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.expect("an answer");
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (code, error, endpoint, length) = (
        response.status().as_u16(),
        header("x-mimic-error"),
        response.headers().contains_key("x-mimic-endpoint"),
        header("content-length"),
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (code, error, endpoint, length, body)
}

#[tokio::test]
async fn flt_11_h2c_an_abort_answers_its_stream_and_the_other_streams_go_on() {
    let _logs = capture_logs();
    let pod = Pod::h2c("pod", 300, None).await;
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(CHECKOUT)]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let sender = h2_client(f.addr).await;
    let get = {
        let sender = sender.clone();
        tokio::spawn(async move { h2_call(&sender, "GET", "/cart").await })
    };
    until("the GET at the pod", || pod.counts.requests() == 1).await;
    for _ in 0..10 {
        let (code, error, endpoint, length, body) = h2_call(&sender, "POST", "/checkout/1").await;
        assert_eq!(code, 503);
        assert_eq!(error.as_deref(), Some("fault-injected"));
        assert!(!endpoint);
        assert_eq!(length.as_deref(), Some("0"));
        assert!(body.is_empty());
    }
    let (code, _, _, _, body) = get.await.unwrap();
    assert_eq!((code, &body[..]), (200, &b"pod"[..]));
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn flt_11_intercepted_tls_an_abort_is_answered_on_the_kept_connection() {
    let logs = capture_logs();
    let rig = common::tls_rig::rig_with(|pod| {
        state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod]).faults(CHECKOUT)])
    })
    .await;
    let mut tls = common::tls_rig::client(&rig).await;
    for _ in 0..2 {
        tls.write_all(
            b"POST /checkout/1 HTTP/1.1\r\nHost: cart.shop.svc\r\ncontent-length: 0\r\n\r\n",
        )
        .await
        .unwrap();
        let head = read_head(&mut tls).await;
        assert_aborted(head.trim_end(), &[], 503);
    }
    let lines = logs.access_exactly(2).await;
    assert!(
        lines
            .iter()
            .all(|l| l["tls"] == true && l["fault"] == "abort")
    );
    assert!(
        rig.pod.received.lock().unwrap().is_empty(),
        "nothing reached the pod"
    );
}

#[tokio::test]
async fn flt_11_configured_statuses_204_304_and_head() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let mut faults: Vec<String> = [200, 418, 599, 204, 304]
        .iter()
        .map(|s| abort_on(&format!("/s{s}"), 100, *s))
        .collect();
    faults.push(
        r#"{"abort":{"percent":100,"status":503},"match":{"method":"HEAD","path":{"type":"Exact","value":"/h"}}}"#
            .to_string(),
    );
    let cart = Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr])
        .faults(&format!("[{}]", faults.join(",")));
    let state = state(&[cart]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for want in [200, 418, 599, 204, 304] {
        let (head, body) = client.get(&format!("/s{want}")).await;
        assert_eq!(status(&head), want, "{head}");
        let head = lower(&head);
        assert_eq!(
            head.matches("x-mimic-error: fault-injected").count(),
            1,
            "{head}"
        );
        if want == 204 || want == 304 {
            assert!(!head.contains("content-length"), "{head}");
        } else {
            assert!(head.contains("content-length: 0"), "{head}");
        }
        assert!(body.is_empty());
    }
    client
        .send(b"HEAD /h HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await;
    let (head, body) = client.read().await.expect("the HEAD answer");
    assert_eq!(status(&head), 503, "{head}");
    assert!(lower(&head).contains("content-length: 0"), "{head}");
    assert!(body.is_empty());
    // A body byte after any of them would break this answer's head.
    let (head, body) = client.get("/cart").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn flt_11_a_1_mib_post_is_aborted_without_reading_its_body() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let cluster = Pod::http1("cluster", 0).await;
    let cart =
        Svc::new("shop/cart", "127.0.0.1", cluster.addr.port(), &[pod.addr]).faults(CHECKOUT);
    let state = state(&[cart]);
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let stream = TcpStream::connect(f.addr).await.unwrap();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(
            b"POST /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 1048576\r\n\r\n",
        )
        .await
        .unwrap();
    // The body is still being written while the answer comes; the write may fail when the
    // sidecar closes.
    let writer = tokio::spawn(async move {
        let _ = write.write_all(&vec![b'x'; 1 << 20]).await;
    });
    let head = lower(&read_head(&mut read).await);
    assert!(head.starts_with("http/1.1 503"), "{head}");
    assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
    assert!(head.contains("x-mimic-error: fault-injected"), "{head}");
    assert!(head.contains("connection: close"), "{head}");
    let _ = writer.await;
    assert_eq!(pod.counts.accepted(), 0);
    assert_eq!(cluster.counts.accepted(), 0);
}

#[tokio::test]
async fn flt_31_http1_an_abort_with_its_body_unread_closes_the_connection() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(CHECKOUT)]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // 1 KiB of a declared 10 KiB, and the app stops sending.
    client
        .send(
            format!(
                "POST /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 10240\r\n\r\n{}",
                "x".repeat(1024)
            )
            .as_bytes(),
        )
        .await;
    let (head, _) = client.read().await.expect("the abort");
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        lower(&head).contains("x-mimic-error: fault-injected"),
        "{head}"
    );
    assert!(lower(&head).contains("connection: close"), "{head}");
    assert!(client.read().await.is_none(), "the connection is closed");
    assert_eq!(pod.counts.accepted(), 0);
}

#[tokio::test]
async fn flt_31_h2c_an_abort_of_a_stream_still_sending_is_reset_no_error() {
    let _logs = capture_logs();
    let pod = Pod::h2c("pod", 0, None).await;
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(CHECKOUT)]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    let (send, conn) = h2::client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut send = send.ready().await.unwrap();
    let request = hyper::Request::post("http://cart.shop.svc/checkout/1")
        .body(())
        .unwrap();
    let (response, mut stream) = send.send_request(request, false).unwrap();
    stream
        .send_data(Bytes::from(vec![b'x'; 1024]), false)
        .unwrap();
    let response = tokio::time::timeout(TIMEOUT, response)
        .await
        .expect("an answer in time")
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["x-mimic-error"], "fault-injected");
    assert!(response.body().is_end_stream(), "HEADERS with END_STREAM");
    let reason = tokio::time::timeout(
        Duration::from_secs(2),
        std::future::poll_fn(|cx| stream.poll_reset(cx)),
    )
    .await
    .expect("the request side reset in time")
    .unwrap();
    assert_eq!(reason, h2::Reason::NO_ERROR);
    // The connection's other streams go on.
    let mut send = send.ready().await.unwrap();
    let request = hyper::Request::get("http://cart.shop.svc/cart")
        .body(())
        .unwrap();
    let (response, _) = send.send_request(request, true).unwrap();
    let response = tokio::time::timeout(TIMEOUT, response)
        .await
        .expect("an answer in time")
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut got = Vec::new();
    while let Some(chunk) = body.data().await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(got, b"pod");
}

/// `n` sequential `GET /` on one connection: how many were aborted `503`.
async fn aborted_of(addr: SocketAddr, n: usize) -> usize {
    let mut client = H1Client::connect(addr).await;
    let mut aborted = 0;
    for _ in 0..n {
        let (head, body) = client.get("/").await;
        if lower(&head).contains("x-mimic-error: fault-injected") {
            assert_aborted(&head, body.as_bytes(), 503);
            aborted += 1;
        } else {
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        }
    }
    aborted
}

#[tokio::test]
async fn flt_12_ten_percent_aborts_about_a_thousand_of_ten_thousand() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let ten = r#"[{"abort":{"percent":10,"status":503}}]"#;
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(ten)]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let aborted = aborted_of(f.addr, 10_000).await;
    assert!((800..=1200).contains(&aborted), "aborted {aborted}");
    assert_eq!(
        pod.counts.requests(),
        10_000 - aborted,
        "the pod got the others"
    );
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_faults_total{",
            &["service=\"shop/cart\"", "kind=\"abort\""]
        ),
        aborted as f64
    );
    // `percent` 0 aborts none and makes no series.
    let pod = Pod::http1("pod", 0).await;
    let zero = r#"[{"abort":{"percent":0,"status":503}}]"#;
    let state = self::state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(zero)]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    assert_eq!(aborted_of(f.addr, 200).await, 0);
    assert_eq!(pod.counts.requests(), 200);
    let scrape = f.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_faults_total"), "{scrape}");
}

#[tokio::test]
async fn flt_13_an_abort_takes_no_try_slot_budget_mirror_or_modifier() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 1500).await;
    let shadow = Pod::http1("shadow", 0).await;
    let to_cart = [backend("shop/cart", 1)];
    let filters = r#"{"mirrors":[{"denominator":100,"numerator":100,"port":80,"service":"shop/shadow"}],"responseHeaders":{"add":[{"name":"x-added","value":"1"}]}}"#;
    let routes = route(&[
        (&exact("/hold"), &to_cart, ""),
        (EVERY_PATH, &to_cart, filters),
    ]);
    let faults = format!(
        "[{},{}]",
        abort_on("/a", 100, 500),
        abort_on("/b", 100, 503)
    );
    let cart = Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr])
        .breaker(1, 1024)
        .outlier(30, 1, 1, 50)
        .faults(&faults)
        .routes(&routes);
    let mirror = Svc::new("shop/shadow", "127.0.0.2", 80, &[shadow.addr]).headless();
    let state = state(&[cart, mirror]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let held = tokio::spawn({
        let addr = f.addr;
        async move { H1Client::connect(addr).await.get("/hold").await }
    });
    until("the slot is held", || {
        pod.counts.in_flight.load(Ordering::SeqCst) == 1
    })
    .await;
    let attempts = |scrape: &str| metric(scrape, "mimic_mesh_attempts_total{", &[]);
    let before = attempts(&f.metrics.encode());
    let mut client = H1Client::connect(f.addr).await;
    for (path, want) in [("/a", 500), ("/b", 503)] {
        for _ in 0..50 {
            let (head, body) = client.get(path).await;
            assert_aborted(&head, body.as_bytes(), want);
            assert!(!lower(&head).contains("x-added"), "{head}");
        }
    }
    let scrape = f.metrics.encode();
    assert_eq!(attempts(&scrape), before, "no try");
    for name in [
        "mimic_mesh_retries_total",
        "mimic_mesh_ejections_total",
        "mimic_mesh_mirrored_total",
    ] {
        assert!(no_series(&scrape, name), "{name}: {scrape}");
    }
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &[
                "reason=\"fault-injected\"",
                "status_class=\"5xx\"",
                "backend=\"shop/cart:80\"",
                "route=\"shop/r\""
            ]
        ),
        100.0
    );
    assert_eq!(state.window_requests_for_test("shop/cart"), 1, "only /hold");
    assert_eq!(state.window_retries_for_test("shop/cart"), 0);
    let (head, body) = held.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
    assert_eq!(shadow.counts.requests(), 0, "no copy");
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn flt_13_an_abort_is_a_synthetic_exchange_and_never_recorded() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr]).faults(CHECKOUT)]);
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..3 {
        client
            .send(b"POST /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 0\r\n\r\n")
            .await;
        let (head, body) = client.read().await.expect("an answer");
        assert_aborted(&head, &body, 503);
    }
    let (head, _) = client.get("/cart").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    until("the GET is recorded", || {
        discovery.report().exchanges() >= 1
    })
    .await;
    let scrape = f.metrics.encode();
    assert!(
        scrape.contains(
            "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 3"
        ),
        "{scrape}"
    );
    let report = discovery.report();
    assert_eq!(report.exchanges(), 1);
    let report = report.to_json().unwrap();
    assert!(!report.contains("/checkout"), "{report}");
}

#[tokio::test]
async fn flt_14_the_access_line_of_an_abort_and_of_a_request_not_drawn() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let faults = format!("[{},{}]", abort_on("/a", 100, 503), abort_on("/n", 0, 503));
    let routes = route(&[(EVERY_PATH, &[backend("shop/cart", 1)], "")]);
    let cart = Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr])
        .faults(&faults)
        .routes(&routes);
    let state = state(&[cart]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for path in ["/a", "/n", "/x"] {
        client.get(path).await;
    }
    let lines = logs.access_exactly(3).await;
    let line = |path: &str| {
        lines
            .iter()
            .find(|l| l["path"] == path)
            .unwrap_or_else(|| panic!("no line of {path}"))
            .clone()
    };
    let a = line("/a");
    assert_eq!(a["fault"], "abort", "{a}");
    assert_eq!(a["reason"], "fault-injected", "{a}");
    assert_eq!(a["status"], 503, "{a}");
    assert_eq!(a["backend"], "shop/cart:80", "{a}");
    assert_eq!(a["route"], "shop/r", "{a}");
    assert_eq!(a["rule"], 0, "{a}");
    for field in ["upstream", "attempts", "fault_delay_ms"] {
        assert!(absent(&a, field), "{field}: {a}");
    }
    for path in ["/n", "/x"] {
        let l = line(path);
        for field in ["fault", "fault_delay_ms", "reason"] {
            assert!(absent(&l, field), "{field}: {l}");
        }
        assert_eq!(l["upstream"], pod.addr.to_string(), "{l}");
    }
    // Unrouted: no route and no rule.
    let unrouted = self::state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[pod.addr])
        .faults(&format!("[{}]", abort_on("/a", 100, 503)))]);
    let f = fixture_mesh(cluster_ip(), &unrouted, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/a").await;
    assert_eq!(status(&head), 503, "{head}");
    let lines = logs.access_exactly(4).await;
    let last = &lines[3];
    assert_eq!(last["fault"], "abort", "{last}");
    assert_eq!(last["backend"], "shop/cart:80", "{last}");
    assert!(absent(last, "route") && absent(last, "rule"), "{last}");
}

#[tokio::test]
async fn flt_21_the_drawn_backends_faults_apply_never_the_parents() {
    let _logs = capture_logs();
    let (own, v1, v2) = (
        Pod::http1("own", 0).await,
        Pod::http1("v1", 0).await,
        Pod::http1("v2", 0).await,
    );
    let all = r#"[{"abort":{"percent":100,"status":503}}]"#;
    let view = |parent: &str, child: &str, backends: &[String]| {
        let routes = route(&[(EVERY_PATH, backends, "")]);
        view_services(&[
            Svc::new("shop/cart", "127.0.0.1", 80, &[own.addr])
                .faults(parent)
                .routes(&routes),
            Svc::new("shop/cart-v1", "127.0.0.3", 80, &[v1.addr])
                .faults(child)
                .headless(),
            Svc::new("shop/cart-v2", "127.0.0.4", 80, &[v2.addr]).headless(),
        ])
    };
    let to_v1 = [backend("shop/cart-v1", 1)];
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(21),
        ..MeshConfig::default()
    });
    state.hold(view(all, "[]", &to_v1), "\"1-a\"");
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    // Phase 1: the parent's faults never apply to what its route sends to cart-v1.
    for _ in 0..50 {
        let (head, body) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200") && body == "v1", "{head}");
    }
    assert!(no_series(&f.metrics.encode(), "mimic_mesh_faults_total"));
    // Phase 2: cart-v1's do.
    state.hold(view("[]", all, &to_v1), "\"2-a\"");
    for _ in 0..50 {
        let (head, body) = client.get("/").await;
        assert_aborted(&head, body.as_bytes(), 503);
    }
    let faults = |scrape: &str, service: &str| {
        metric(
            scrape,
            "mimic_mesh_faults_total{",
            &[&format!("service=\"{service}\""), "kind=\"abort\""],
        )
    };
    let injected = |scrape: &str| {
        metric(
            scrape,
            "mimic_mesh_requests_total{",
            &["backend=\"shop/cart-v1:80\"", "reason=\"fault-injected\""],
        )
    };
    let scrape = f.metrics.encode();
    assert_eq!(faults(&scrape, "shop/cart-v1"), 50.0);
    assert_eq!(faults(&scrape, "shop/cart"), 0.0);
    let (faults_before, injected_before) = (faults(&scrape, "shop/cart-v1"), injected(&scrape));
    // Phase 3: 50/50 to cart-v1 (aborting) and cart-v2.
    state.hold(
        view(
            "[]",
            all,
            &[backend("shop/cart-v1", 1), backend("shop/cart-v2", 1)],
        ),
        "\"3-a\"",
    );
    let v1_before = v1.counts.requests();
    let aborted = aborted_of(f.addr, 2000).await;
    assert!((900..=1100).contains(&aborted), "aborted {aborted}");
    assert_eq!(v2.counts.requests(), 2000 - aborted);
    assert_eq!(v1.counts.requests(), v1_before, "cart-v1 gets none");
    let scrape = f.metrics.encode();
    assert_eq!(injected(&scrape) - injected_before, aborted as f64);
    assert_eq!(
        faults(&scrape, "shop/cart-v1") - faults_before,
        aborted as f64
    );
    assert_eq!(own.counts.requests(), 0);
}

#[tokio::test]
async fn flt_21_the_sidecars_own_route_answers_carry_no_fault() {
    let _logs = capture_logs();
    let own = Pod::http1("own", 0).await;
    let routes = route(&[
        (&exact("/none"), &[], ""),
        (
            &exact("/invalid"),
            &[r#"{"invalid":true,"weight":1}"#.to_string()],
            "",
        ),
    ]);
    let cart = Svc::new("shop/cart", "127.0.0.1", 80, &[own.addr])
        .faults(r#"[{"abort":{"percent":100,"status":503}}]"#)
        .routes(&routes);
    let state = state(&[cart]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for (path, want, reason) in [
        ("/other", 404, "no-route"),
        ("/none", 500, "no-backend"),
        ("/invalid", 500, "invalid-backend"),
    ] {
        let (head, _) = client.get(path).await;
        assert_eq!(status(&head), want, "{path}: {head}");
        let head = lower(&head);
        assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
        assert!(head.contains(&format!("x-mimic-error: {reason}")), "{head}");
    }
    let scrape = f.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_faults_total"), "{scrape}");
    assert_eq!(own.counts.requests(), 0);
}

#[tokio::test]
async fn flt_24_a_forwarded_fault_injected_is_an_app_answer() {
    let logs = capture_logs();
    let reply = |_: &[u8]| {
        b"HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: fault-injected\r\ncontent-length: 0\r\n\r\n"
            .to_vec()
    };
    let (a, b) = (RawPod::start(reply).await, RawPod::start(reply).await);
    let state = state(&[Svc::new("shop/cart", "127.0.0.1", 80, &[a.addr, b.addr])]);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        let (head, _) = client.get("/").await;
        assert_eq!(status(&head), 503, "{head}");
        let head = lower(&head);
        assert_eq!(head.matches("x-mimic-error").count(), 1, "{head}");
        assert!(head.contains("x-mimic-error: fault-injected"), "{head}");
    }
    let heads = |p: &RawPod| {
        String::from_utf8_lossy(&p.received())
            .matches("GET / HTTP/1.1")
            .count()
    };
    assert_eq!(heads(&a) + heads(&b), 10, "one try each, never retried");
    let scrape = f.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_faults_total"), "{scrape}");
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_requests_total{",
            &["reason=\"none\"", "status_class=\"5xx\""]
        ),
        10.0
    );
    for line in logs.access_exactly(10).await {
        assert!(absent(&line, "reason") && absent(&line, "fault"), "{line}");
    }
}
