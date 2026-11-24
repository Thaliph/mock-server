//! The delay half of FLT-14, FLT-15 to FLT-20 and FLT-32: the delays of the caller's sidecar
//! (spec §7.9 "Fault injection", §12.28): waited inside the breaker slot before the first try,
//! once per request, raced against `requestMs` and the app leaving, holding no body byte and no
//! pod, followed by the mirrors, never an outlier outcome, and bounded by the grace.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::faults::{abort_on, absent, delay, faults, no_series, status};
use common::mesh_pods::{H1Client, Pod, RawPod, Svc, cluster_ip, mesh_state};
use common::{capture_logs, closed_port, fixture_mesh, h2_client, median, metric, until};
use http_body_util::{BodyExt, Full};
use mimic_core::discovery::histogram::bucket_index;
use mimic_core::mesh::fault::drawn;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MS: Duration = Duration::from_millis(1);

/// `shop/cart` on the ClusterIP the app dials, its pods `pods`.
fn cart(pods: &[SocketAddr]) -> Svc {
    Svc::new("shop/cart", "127.0.0.1", 80, pods)
}

/// The route `shop/r`: every path to `shop/cart`, with these `filters` and `timeouts` (JSON
/// objects, none when empty).
fn route(filters: &str, timeouts: &str) -> String {
    let filters = if filters.is_empty() {
        String::new()
    } else {
        format!(r#","filters":{filters}"#)
    };
    let timeouts = if timeouts.is_empty() {
        String::new()
    } else {
        format!(r#","timeouts":{timeouts}"#)
    };
    format!(
        r#"{{"name":"shop/r","rules":[{{"backends":[{{"port":80,"service":"shop/cart","weight":1}}]{filters},"matches":[{{"path":{{"type":"PathPrefix","value":"/"}}}}]{timeouts}}}]}}"#
    )
}

const MIRROR: &str =
    r#"{"mirrors":[{"denominator":100,"numerator":100,"port":80,"service":"shop/shadow"}]}"#;

/// `GET path` on `client`: the head, the body and the client's latency.
async fn timed_get(client: &mut H1Client, path: &str) -> (String, String, Duration) {
    let began = Instant::now();
    let (head, body) = client.get(path).await;
    (head, body, began.elapsed())
}

/// Asserts each request reached the pod at least 200 ms after it was sent, and the median
/// latency is within 200 and 350 ms (two delays would give at least 400 ms).
fn assert_held(sent: &[Instant], arrivals: &[Instant], latencies: Vec<u64>) {
    assert_eq!(sent.len(), arrivals.len());
    for (i, (s, a)) in sent.iter().zip(arrivals).enumerate() {
        let held = a.duration_since(*s);
        assert!(
            held >= 200 * MS,
            "request {i} reached the pod after {held:?}"
        );
    }
    let m = median(latencies.clone());
    assert!((200..350).contains(&m), "median {m} of {latencies:?}");
}

#[tokio::test]
async fn flt_15_http1_a_200ms_delay_holds_every_request_before_the_pod() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(&[pod.addr]).faults(&delay(100, 200))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (mut sent, mut latencies) = (Vec::new(), Vec::new());
    for _ in 0..50 {
        sent.push(Instant::now());
        let (head, body, took) = timed_get(&mut client, "/").await;
        assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
        latencies.push(took.as_millis() as u64);
    }
    assert_held(&sent, &pod.counts.arrivals(), latencies);
    assert_eq!(faults(&f, "delay"), 50.0);
}

#[tokio::test]
async fn flt_15_h2c_a_200ms_delay_holds_every_stream() {
    let _logs = capture_logs();
    let pod = Pod::h2c("pod", 0, None).await;
    let state = mesh_state(&[cart(&[pod.addr]).faults(&delay(100, 200))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut sender = h2_client(f.addr).await;
    let (mut sent, mut latencies) = (Vec::new(), Vec::new());
    for _ in 0..20 {
        let request = hyper::Request::get("http://cart.shop.svc/")
            .body(Full::new(Bytes::new()))
            .unwrap();
        sent.push(Instant::now());
        let response = sender.send_request(request).await.expect("an answer");
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        latencies.push(sent.last().unwrap().elapsed().as_millis() as u64);
        assert_eq!(&body[..], b"pod");
    }
    assert_held(&sent, &pod.counts.arrivals(), latencies);
    assert_eq!(faults(&f, "delay"), 20.0);
}

/// `clients` keep-alive clients each sending `each` `GET /` at once: every answer's head and
/// latency.
async fn concurrent(addr: SocketAddr, clients: usize, each: usize) -> Vec<(String, Duration)> {
    let tasks: Vec<_> = (0..clients)
        .map(|_| {
            tokio::spawn(async move {
                let mut client = H1Client::connect(addr).await;
                let mut out = Vec::new();
                for _ in 0..each {
                    let (head, _, took) = timed_get(&mut client, "/").await;
                    out.push((head, took));
                }
                out
            })
        })
        .collect();
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all
}

#[tokio::test]
async fn flt_15_ten_percent_delays_about_two_hundred_of_two_thousand() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(&[pod.addr]).faults(&delay(10, 200))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let answers = concurrent(f.addr, 20, 100).await;
    assert!(answers.iter().all(|(h, _)| h.starts_with("HTTP/1.1 200")));
    let (delayed, others): (Vec<_>, Vec<_>) = answers
        .iter()
        .map(|(_, took)| *took)
        .partition(|took| *took >= 200 * MS);
    assert!((160..=240).contains(&delayed.len()), "{}", delayed.len());
    assert_eq!(faults(&f, "delay"), delayed.len() as f64);
    let others: Vec<u64> = others.iter().map(|t| t.as_millis() as u64).collect();
    assert!(others.iter().all(|ms| *ms < 150), "{others:?}");
    assert!(median(others.clone()) < 50, "{others:?}");
}

#[tokio::test]
async fn flt_15_an_abort_drawn_wins_over_a_delay() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let both = r#"[{"abort":{"percent":50,"status":503},"delay":{"fixedMs":200,"percent":100}}]"#;
    let state = mesh_state(&[cart(&[pod.addr]).faults(both)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let answers = concurrent(f.addr, 20, 20).await;
    let (aborted, answered): (Vec<_>, Vec<_>) = answers
        .iter()
        .partition(|(h, _)| h.to_ascii_lowercase().contains("fault-injected"));
    assert!((180..=220).contains(&aborted.len()), "{}", aborted.len());
    let aborted_ms: Vec<u64> = aborted.iter().map(|(_, t)| t.as_millis() as u64).collect();
    assert!(aborted_ms.iter().all(|ms| *ms < 150), "{aborted_ms:?}");
    assert!(median(aborted_ms.clone()) <= 10, "{aborted_ms:?}");
    for (head, took) in &answered {
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(*took >= 200 * MS, "{took:?}");
    }
    assert_eq!(faults(&f, "delay"), answered.len() as f64);
    assert_eq!(faults(&f, "abort"), aborted.len() as f64);
    assert_eq!(pod.counts.requests(), answered.len());
}

#[tokio::test]
async fn flt_15_the_abort_and_the_delay_are_drawn_from_their_own_values_in_order() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let both = r#"[{"abort":{"percent":10,"status":503},"delay":{"fixedMs":5,"percent":20}}]"#;
    let state = mesh_state(&[cart(&[pod.addr]).faults(both)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut expected = Vec::new();
    for i in 0..500 {
        // The decision's two values are the next two draws: the abort's first.
        let (r_abort, r_delay) = (state.peek_random_for_test(1), state.peek_random_for_test(2));
        let want = if drawn(r_abort, 10) {
            "abort"
        } else if drawn(r_delay, 20) {
            "delay"
        } else {
            "none"
        };
        expected.push(want);
        let (head, _) = client.get(&format!("/r{i}")).await;
        assert_eq!(status(&head) == 503, want == "abort", "request {i}: {head}");
    }
    let lines = logs.access_exactly(500).await;
    for (i, want) in expected.iter().enumerate() {
        let path = format!("/r{i}");
        let line = lines.iter().find(|l| l["path"] == path).unwrap();
        let got = line["fault"].as_str().unwrap_or("none");
        assert_eq!(got, *want, "request {i}: fault {got}");
    }
    let count = |k| expected.iter().filter(|w| **w == k).count();
    // 10 % aborts and 18 % delays (`20 × (1 − 0.1)`) of 500.
    assert!((30..=70).contains(&count("abort")), "{}", count("abort"));
    assert!((60..=120).contains(&count("delay")), "{}", count("delay"));
    assert_eq!(faults(&f, "delay"), count("delay") as f64);
}

#[tokio::test]
async fn flt_16_a_delayed_post_holds_no_body_byte_no_pod_and_two_timers() {
    let _logs = capture_logs();
    let pod = RawPod::start(|_| b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()).await;
    let svc = cart(&[pod.addr])
        .faults(&delay(100, 1000))
        .routes(&route("", r#"{"requestMs":5000}"#));
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let body: Vec<u8> = (0..32768u32).map(|i| (i % 251) as u8).collect();
    let mut request =
        b"POST /p HTTP/1.1\r\nHost: cart.shop.svc:80\r\ncontent-length: 32768\r\n\r\n".to_vec();
    request.extend_from_slice(&body);
    let mut stream = TcpStream::connect(f.addr).await.unwrap();
    let began = Instant::now();
    stream.write_all(&request).await.unwrap();
    tokio::time::sleep(300 * MS).await;
    let scrape = f.metrics.encode();
    assert!(pod.received().is_empty(), "the pod got something");
    assert_eq!(metric(&scrape, "mimic_mesh_retry_buffer_bytes", &[]), 0.0);
    assert_eq!(metric(&scrape, "mimic_mesh_endpoint_in_flight{", &[]), 0.0);
    assert_eq!(
        state.armed_timers_for_test(),
        2,
        "the route's and the delay's"
    );
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !got.ends_with(b"ok") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .expect("an answer in time")
            .unwrap();
        assert!(n > 0, "closed before the answer");
        got.extend_from_slice(&buf[..n]);
    }
    assert!(began.elapsed() >= 1000 * MS);
    assert!(got.starts_with(b"HTTP/1.1 200"));
    let received = pod.received();
    let end = received.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(received.starts_with(b"POST /p HTTP/1.1\r\n"));
    assert!(received[end..] == body[..], "the body byte for byte");
}

#[tokio::test]
async fn flt_16_delayed_requests_hold_the_slots_and_a_leaving_client_frees_one() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let svc = cart(&[pod.addr]).faults(&delay(100, 1000)).breaker(2, 1024);
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let kept = tokio::spawn({
        let addr = f.addr;
        async move { timed_get(&mut H1Client::connect(addr).await, "/kept").await }
    });
    let mut leaving = TcpStream::connect(f.addr).await.unwrap();
    leaving
        .write_all(b"GET /leaving HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await
        .unwrap();
    until("two delays", || faults(&f, "delay") == 2.0).await;
    let (head, _, took) = timed_get(&mut H1Client::connect(f.addr).await, "/third").await;
    assert_eq!(status(&head), 503, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("x-mimic-error: breaker-open"),
        "{head}"
    );
    assert!(took < 150 * MS, "{took:?}");
    assert_eq!(faults(&f, "delay"), 2.0, "the refused one is not delayed");
    assert_eq!(state.armed_timers_for_test(), 2);
    drop(leaving);
    let left = Instant::now();
    while state.armed_timers_for_test() != 1 {
        assert!(left.elapsed() < 250 * MS, "the timer was not freed");
        tokio::time::sleep(5 * MS).await;
    }
    let (head, _, took) = timed_get(&mut H1Client::connect(f.addr).await, "/new").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(took >= 1000 * MS, "{took:?}");
    let (head, ..) = kept.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(pod.counts.requests(), 2, "the leaver reached no pod");
    let lines = logs.access_exactly(4).await;
    let line = |path: &str| lines.iter().find(|l| l["path"] == path).unwrap().clone();
    assert!(absent(&line("/third"), "fault"), "{}", line("/third"));
    let gone = line("/leaving");
    assert_eq!(gone["result"], "client_abort", "{gone}");
    assert_eq!(gone["fault"], "delay", "{gone}");
}

#[tokio::test]
async fn flt_16_an_h2c_stream_reset_during_the_delay_frees_its_slot() {
    let _logs = capture_logs();
    let pod = Pod::h2c("pod", 0, None).await;
    let svc = cart(&[pod.addr]).faults(&delay(100, 1000)).breaker(1, 1024);
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let sender = h2_client(f.addr).await;
    let call = |sender: &common::H2Sender| {
        let mut sender = sender.clone();
        tokio::spawn(async move {
            let began = Instant::now();
            let request = hyper::Request::get("http://cart.shop.svc/")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = sender.send_request(request).await.expect("an answer");
            (response.status().as_u16(), began.elapsed())
        })
    };
    let pending = call(&sender);
    until("the delay", || faults(&f, "delay") == 1.0).await;
    // Dropping the pending request resets its stream.
    pending.abort();
    let left = Instant::now();
    while state.armed_timers_for_test() != 0 {
        assert!(left.elapsed() < 250 * MS, "the timer was not freed");
        tokio::time::sleep(5 * MS).await;
    }
    let (code, took) = call(&sender).await.unwrap();
    assert_eq!(code, 200, "the slot was freed");
    assert!(took >= 1000 * MS, "{took:?}");
    assert_eq!(pod.counts.requests(), 1, "the reset stream reached no pod");
}

#[tokio::test]
async fn flt_16_a_port_without_endpoints_answers_no_endpoint_without_the_delay() {
    let logs = capture_logs();
    let state = mesh_state(&[cart(&[]).faults(&delay(100, 1000))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _, took) = timed_get(&mut H1Client::connect(f.addr).await, "/").await;
    assert_eq!(status(&head), 503, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("x-mimic-error: no-endpoint"),
        "{head}"
    );
    assert!(took < 150 * MS, "{took:?}");
    assert!(no_series(&f.metrics.encode(), "mimic_mesh_faults_total"));
    let lines = logs.access_exactly(1).await;
    assert!(absent(&lines[0], "fault"), "{}", lines[0]);
}

#[tokio::test]
async fn flt_17_a_request_timeout_during_the_delay_answers_504_without_a_try() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let svc = cart(&[pod.addr])
        .faults(&delay(100, 500))
        .routes(&route("", r#"{"requestMs":100}"#));
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let began = Instant::now();
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    let took = began.elapsed();
    assert_eq!(status(&head), 504, "{head}");
    assert!(
        head.to_ascii_lowercase().contains("x-mimic-error: timeout"),
        "{head}"
    );
    assert!(took >= 100 * MS && took < 250 * MS, "{took:?}");
    tokio::time::sleep((600 * MS).saturating_sub(began.elapsed())).await;
    assert_eq!(pod.counts.requests(), 0);
    let scrape = f.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_attempts_total"), "{scrape}");
    assert_eq!(state.window_requests_for_test("shop/cart"), 0);
    assert_eq!(faults(&f, "delay"), 1.0);
    let lines = logs.access_exactly(1).await;
    let l = &lines[0];
    assert_eq!(l["result"], "timeout", "{l}");
    assert_eq!(l["timeout"], "request", "{l}");
    assert_eq!(l["fault"], "delay", "{l}");
    assert_eq!(l["fault_delay_ms"], 500, "{l}");
    assert!(absent(l, "upstream") && absent(l, "attempts"), "{l}");
}

#[tokio::test]
async fn flt_17_backend_request_does_not_count_the_delay() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 10).await;
    let svc = cart(&[pod.addr])
        .faults(&delay(100, 500))
        .routes(&route("", r#"{"backendRequestMs":100}"#));
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, body, took) = timed_get(&mut H1Client::connect(f.addr).await, "/").await;
    assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
    assert!(took >= 500 * MS, "{took:?}");
}

#[tokio::test]
async fn flt_17_request_counts_the_delay_and_the_try_together() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 300).await;
    let svc = cart(&[pod.addr])
        .faults(&delay(100, 500))
        .routes(&route("", r#"{"requestMs":700}"#));
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _, took) = timed_get(&mut H1Client::connect(f.addr).await, "/").await;
    assert_eq!(status(&head), 504, "{head}");
    assert!(took >= 700 * MS, "{took:?}");
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn flt_18_one_delay_per_request_whatever_its_tries() {
    let logs = capture_logs();
    let (a, b) = (Pod::http1("a", 0).await, Pod::http1("b", 0).await);
    let svc = cart(&[closed_port().await, a.addr, b.addr])
        .faults(&delay(100, 200))
        .outlier(30, 0, 100, 50)
        .budget(100, 100, 10);
    let state = mesh_state(&[svc], |c| c.avoid_failed_for = MS);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..30 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    let lines = logs.access_exactly(30).await;
    let retried: Vec<_> = lines.iter().filter(|l| l["attempts"] == 2).collect();
    assert!(!retried.is_empty(), "no first try failed to connect");
    for l in retried {
        assert_eq!(l["status"], 200, "{l}");
        assert!(l["duration_ms"].as_u64().unwrap() < 350, "{l}");
    }
    assert_eq!(faults(&f, "delay"), 30.0);
    assert_eq!(state.window_requests_for_test("shop/cart"), 30);
}

#[tokio::test]
async fn flt_18_a_request_that_falls_back_to_the_cluster_ip_is_delayed_once() {
    let logs = capture_logs();
    let cluster = Pod::http1("cluster", 0).await;
    let pods = [
        closed_port().await,
        closed_port().await,
        closed_port().await,
    ];
    let svc = Svc::new("shop/cart", "127.0.0.1", cluster.addr.port(), &pods)
        .faults(&delay(100, 200))
        .outlier(30, 0, 100, 50)
        .budget(100, 100, 10);
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster.addr, &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..5 {
        let (head, body, took) = timed_get(&mut client, "/").await;
        assert!(
            head.starts_with("HTTP/1.1 200") && body == "cluster",
            "{head}"
        );
        assert!(took < 350 * MS, "{took:?}");
    }
    for l in logs.access_exactly(5).await {
        assert_eq!(l["attempts"], 4, "{l}");
    }
    let scrape = f.metrics.encode();
    let label = format!("shop/cart:{}", cluster.addr.port());
    assert_eq!(faults(&f, "delay"), 5.0);
    assert_eq!(
        metric(&scrape, "mimic_mesh_fallbacks_total{", &[&label]),
        5.0
    );
}

#[tokio::test]
async fn flt_19_copies_follow_the_primary_after_the_delay() {
    let logs = capture_logs();
    let (primary, shadow) = (
        Pod::http1("primary", 0).await,
        Pod::http1("shadow", 0).await,
    );
    let svc = cart(&[primary.addr])
        .faults(&delay(100, 200))
        .routes(&route(MIRROR, ""));
    let mirror = Svc::new("shop/shadow", "127.0.0.2", 80, &[shadow.addr]).headless();
    let state = mesh_state(&[svc, mirror], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for i in 0..10 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        until("the copy", || shadow.counts.requests() == i + 1).await;
    }
    let (p, s) = (primary.counts.arrivals(), shadow.counts.arrivals());
    for i in 0..10 {
        assert!(s[i] >= p[i], "copy {i} came before its primary");
    }
    for l in logs.access_exactly(10).await {
        assert_eq!(l["mirrored"], true, "{l}");
    }
}

#[tokio::test]
async fn flt_19_a_mirror_deadline_shorter_than_the_delay_makes_every_copy_an_error() {
    let _logs = capture_logs();
    let (primary, shadow) = (
        Pod::http1("primary", 0).await,
        Pod::http1("shadow", 0).await,
    );
    let svc = cart(&[primary.addr])
        .faults(&delay(100, 1000))
        .routes(&route(MIRROR, ""));
    let mirror = Svc::new("shop/shadow", "127.0.0.2", 80, &[shadow.addr]).headless();
    let state = mesh_state(&[svc, mirror], |c| c.mirror_timeout = 500 * MS);
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let calls: Vec<_> = (0..3)
        .map(|_| {
            let addr = f.addr;
            tokio::spawn(async move { H1Client::connect(addr).await.get("/").await })
        })
        .collect();
    for c in calls {
        let (head, _) = c.await.unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    until("three copies in error", || {
        metric(
            &f.metrics.encode(),
            "mimic_mesh_mirrored_total{",
            &["result=\"error\""],
        ) == 3.0
    })
    .await;
    assert_eq!(shadow.counts.requests(), 0);
}

#[tokio::test]
async fn flt_19_copies_are_never_faulted() {
    let _logs = capture_logs();
    let (primary, shadow) = (
        Pod::http1("primary", 0).await,
        Pod::http1("shadow", 0).await,
    );
    let svc = cart(&[primary.addr]).routes(&route(MIRROR, ""));
    let mirror = Svc::new("shop/shadow", "127.0.0.2", 80, &[shadow.addr])
        .headless()
        .faults(
            r#"[{"abort":{"percent":100,"status":503},"delay":{"fixedMs":5000,"percent":100}}]"#,
        );
    let state = mesh_state(&[svc, mirror], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..10 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    until("every copy", || shadow.counts.requests() == 10).await;
    assert!(no_series(&f.metrics.encode(), "mimic_mesh_faults_total"));
}

#[tokio::test]
async fn flt_20_aborts_and_delays_eject_no_pod() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let faults = r#"[{"abort":{"percent":100,"status":500},"match":{"path":{"type":"Exact","value":"/500"}}},{"abort":{"percent":100,"status":503},"match":{"path":{"type":"Exact","value":"/503"}}},{"delay":{"fixedMs":20,"percent":100}}]"#;
    let state = mesh_state(
        &[cart(&[pod.addr]).faults(faults).outlier(30, 1, 1, 50)],
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for (path, want) in [("/500", 500), ("/503", 503), ("/", 200)] {
        for _ in 0..100 {
            let (head, _) = client.get(path).await;
            assert_eq!(status(&head), want, "{head}");
        }
    }
    let scrape = f.metrics.encode();
    assert!(no_series(&scrape, "mimic_mesh_ejections_total"), "{scrape}");
    assert_eq!(metric(&scrape, "mimic_mesh_ejected_endpoints{", &[]), 0.0);
    assert_eq!(pod.counts.requests(), 100);
}

#[tokio::test]
async fn flt_20_a_delayed_try_that_is_reset_counts_one_failure() {
    let logs = capture_logs();
    let resetting = RawPod::start(|_| Vec::new()).await;
    let ok = Pod::http1("ok", 0).await;
    let svc = cart(&[resetting.addr, ok.addr])
        .faults(&delay(100, 20))
        .outlier(30, 0, 1, 50);
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..50 {
        let (head, _) = client.get("/").await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        if resetting.counts.requests() > 0 {
            break;
        }
    }
    assert_eq!(
        resetting.counts.requests(),
        1,
        "the resetting pod saw a head"
    );
    let scrape = f.metrics.encode();
    assert_eq!(
        metric(
            &scrape,
            "mimic_mesh_ejections_total{",
            &[
                "backend=\"shop/cart:80\"",
                "reason=\"consecutive_failures\""
            ]
        ),
        1.0
    );
    let ejected = logs.messages("endpoint ejected").await;
    assert_eq!(ejected.len(), 1);
    assert_eq!(ejected[0]["endpoint"], resetting.addr.to_string());
}

#[tokio::test]
async fn flt_14_the_access_line_of_a_delayed_request() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let svc = cart(&[pod.addr])
        .faults(&delay(100, 50))
        .routes(&route("", ""));
    let state = mesh_state(&[svc], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let (head, _) = H1Client::connect(f.addr).await.get("/").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let lines = logs.access_exactly(1).await;
    let l = &lines[0];
    assert_eq!(l["fault"], "delay", "{l}");
    assert_eq!(l["fault_delay_ms"], 50, "{l}");
    assert_eq!(l["upstream"], pod.addr.to_string(), "{l}");
    assert_eq!(l["attempts"], 1, "{l}");
    assert_eq!(l["route"], "shop/r", "{l}");
    assert!(l["duration_ms"].as_u64().unwrap() >= 50, "{l}");
    assert!(l["head_ms"].as_u64().unwrap() >= 50, "{l}");
    assert!(absent(l, "reason"), "{l}");
}

#[tokio::test]
async fn flt_32_a_delay_in_flight_at_sigterm_ends_within_the_grace() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(&[pod.addr]).faults(&delay(100, 500))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let call = tokio::spawn({
        let addr = f.addr;
        async move { timed_get(&mut H1Client::connect(addr).await, "/").await }
    });
    until("the delay", || faults(&f, "delay") == 1.0).await;
    f.shared.shutdown();
    let left = f.shared.drain(Duration::from_secs(2)).await;
    let (head, _, took) = call.await.unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(took >= 500 * MS, "the delay was not shortened: {took:?}");
    assert_eq!(left, 0, "nothing was cut");
    assert_eq!(pod.counts.requests(), 1);
}

#[tokio::test]
async fn flt_32_a_delay_longer_than_the_grace_is_cut_at_its_end() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(&[pod.addr]).faults(&delay(100, 3000))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let began = Instant::now();
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET / HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n")
        .await;
    until("the delay", || faults(&f, "delay") == 1.0).await;
    f.shared.shutdown();
    assert_eq!(f.shared.drain(500 * MS).await, 1, "cut at the grace's end");
    assert!(client.read().await.is_none(), "the connection was closed");
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["result"], "shutdown", "{}", lines[0]);
    assert_eq!(lines[0]["fault"], "delay", "{}", lines[0]);
    tokio::time::sleep((3300 * MS).saturating_sub(began.elapsed())).await;
    assert_eq!(pod.counts.requests(), 0);
    assert_eq!(pod.counts.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn flt_25_discovery_records_the_latency_without_the_delay() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 10).await;
    let faults = format!(
        r#"[{},{{"delay":{{"fixedMs":300,"percent":100}}}}]"#,
        abort_on("/abort", 100, 503)
    );
    let state = mesh_state(&[cart(&[pod.addr]).faults(&faults)], |_| {});
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_mesh(cluster_ip(), &state, Some(&discovery)).await;
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..2 {
        let (head, _) = client.get("/abort").await;
        assert_eq!(status(&head), 503, "{head}");
    }
    for _ in 0..5 {
        let (head, body) = client.get("/users/1").await;
        assert!(head.starts_with("HTTP/1.1 200") && body == "pod", "{head}");
    }
    until("the GETs are recorded", || {
        discovery.report().exchanges() >= 5
    })
    .await;
    let report = discovery.report();
    assert_eq!(report.exchanges(), 5);
    for endpoint in report.endpoints() {
        let buckets: Vec<u8> = endpoint.stats.latency.buckets().keys().copied().collect();
        assert!(
            buckets.iter().all(|b| *b < bucket_index(100)),
            "{buckets:?}"
        );
    }
    let json = report.to_json().unwrap();
    assert!(
        !json.contains("/abort"),
        "the report holds the aborted path"
    );
    let lines = logs.access_exactly(7).await;
    let delayed: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["path"] == "/users/1").collect();
    assert_eq!(delayed.len(), 5);
    for l in delayed {
        assert_eq!(l["fault_delay_ms"], 300, "{l}");
        assert!(l["head_ms"].as_u64().unwrap() >= 300, "{l}");
    }
    let scrape = f.metrics.encode();
    assert!(
        scrape.contains(
            "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"synthetic\"} 2"
        ),
        "{scrape}"
    );
}
