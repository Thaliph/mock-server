//! FLT-22, FLT-23 and FLT-26: what the faults of the caller's sidecar match and what they never
//! touch (spec §7.9 "Fault injection", §12.28): the request as the app sent it and the first
//! fault that holds, the paths that are never faulted, a new view applying from the next
//! request, and the `mimic_mesh_faults_total` series and their eviction, without request data.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::faults::{abort_on, absent, delay, faults, no_series, status};
use common::mesh_pods::{
    H1Client, Pod, RawPod, Svc, cluster_ip, mesh_state, view_services, view_text,
};
use common::{Fixture, capture_logs, fixture_mesh, fixture_mesh_dst, until};

/// `abort` 100 % on every request.
const ALL: &str = r#"[{"abort":{"percent":100,"status":503}}]"#;

/// The faults of FLT-22: an abort of the `POST`s under `/checkout`, a delay of 50 ms of the
/// requests with `x-chaos: on`, then a delay of 10 ms of any other.
const F22: &str = r#"[{"abort":{"percent":100,"status":503},"match":{"method":"POST","path":{"type":"PathPrefix","value":"/checkout"}}},{"delay":{"fixedMs":50,"percent":100},"match":{"headers":[{"name":"x-chaos","value":"on"}]}},{"delay":{"fixedMs":10,"percent":100}}]"#;

/// The route `shop/r`: every path to `shop/cart`, its request modifier setting `x-chaos: on`.
const CHAOS_ROUTE: &str = r#"{"name":"shop/r","rules":[{"backends":[{"port":80,"service":"shop/cart","weight":1}],"filters":{"requestHeaders":{"set":[{"name":"x-chaos","value":"on"}]}},"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}"#;

const UPGRADE: &[u8] =
    b"GET /ws HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";

/// `shop/cart` on the ClusterIP `port` of `127.0.0.1`, its pods `pods`.
fn cart(port: u16, pods: &[SocketAddr]) -> Svc {
    Svc::new("shop/cart", "127.0.0.1", port, pods)
}

/// A `&'static str` for the keys and IPs of the `Svc`s made in a loop.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Whether the head is the answer of an abort of `want`.
fn aborted(head: &str, want: u16) -> bool {
    status(head) == want
        && head
            .to_ascii_lowercase()
            .contains("x-mimic-error: fault-injected")
}

/// A `200` from `pod` (a `Pod` answers with its name).
fn from(head: &str, body: &str, pod: &str) -> bool {
    head.starts_with("HTTP/1.1 200") && body == pod
}

/// The request heads `received` holds, in order.
fn heads(received: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(received)
        .split("\r\n\r\n")
        .filter(|h| !h.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[tokio::test]
async fn flt_22_the_first_fault_that_holds_decides_on_the_request_as_the_app_sent_it() {
    let logs = capture_logs();
    let pod = RawPod::start(|_| b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()).await;
    let state = mesh_state(
        &[cart(80, &[pod.addr]).faults(F22).routes(CHAOS_ROUTE)],
        |_| {},
    );
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let mut ask = async |request: &str| {
        let began = Instant::now();
        client.send(request.as_bytes()).await;
        let (head, _) = client.read().await.expect("an answer");
        (head, began.elapsed())
    };
    // ① and ②: the abort is the first item, so it wins over the delay that holds as well.
    for chaos in ["", "x-chaos: on\r\n"] {
        let (head, _) = ask(&format!(
            "POST /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\n{chaos}content-length: 0\r\n\r\n"
        ))
        .await;
        assert!(aborted(&head, 503), "{head}");
    }
    // ③: header names are matched without their case.
    let (head, took) =
        ask("GET /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\nX-Chaos: on\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(took >= Duration::from_millis(50), "{took:?}");
    // ④: the modifier sets `x-chaos: on` after the decision.
    let (head, _) = ask("GET /cart HTTP/1.1\r\nHost: cart.shop.svc:80\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // ⑤: the two lines are one value, `on,x`.
    let (head, _) = ask(
        "GET /checkout HTTP/1.1\r\nHost: cart.shop.svc:80\r\nx-chaos: on\r\nx-chaos: x\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let lines = logs.access_exactly(5).await;
    for (i, line) in lines[..2].iter().enumerate() {
        assert_eq!(
            line["fault"], "abort",
            "request {i}: fault {}",
            line["fault"]
        );
    }
    for (i, want) in [(2, 50), (3, 10), (4, 10)] {
        let line = &lines[i];
        assert_eq!(
            line["fault"], "delay",
            "request {i}: fault {}",
            line["fault"]
        );
        assert_eq!(
            line["fault_delay_ms"], want,
            "request {i}: fault_delay_ms {}",
            line["fault_delay_ms"]
        );
    }
    let heads = heads(&pod.received());
    assert_eq!(heads.len(), 3, "{heads:?}");
    for head in &heads {
        let chaos: Vec<&str> = head.lines().filter(|l| l.starts_with("x-chaos:")).collect();
        assert_eq!(chaos, ["x-chaos: on"], "{head}");
    }
}

#[tokio::test]
async fn flt_22_a_query_match_behaves_as_a_route_match() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let list = r#"[{"abort":{"percent":100,"status":418},"match":{"queryParams":[{"name":"v","value":"2"}]}}]"#;
    let state = mesh_state(&[cart(80, &[pod.addr]).faults(list)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    for target in ["/p?v=2", "/p?v=2&v=3", "/p?v=%32", "/p?v=%zz&v=2"] {
        let (head, _) = client.get(target).await;
        assert!(aborted(&head, 418), "{target}: {head}");
    }
    for target in ["/p?v=3&v=2", "/p?V=2", "/p"] {
        let (head, body) = client.get(target).await;
        assert!(from(&head, &body, "pod"), "{target}: {head}");
    }
    assert_eq!(pod.counts.requests(), 3);
    assert_eq!(faults(&f, "abort"), 4.0);
}

#[tokio::test]
async fn flt_23_out_of_view_stale_and_mesh_false_reach_their_destination() {
    let _logs = capture_logs();
    let (pod, cluster, dest) = (
        Pod::http1("pod", 0).await,
        Pod::http1("cluster", 0).await,
        Pod::http1("dest", 0).await,
    );
    let state = mesh_state(&[cart(cluster.addr.port(), &[pod.addr]).faults(ALL)], |c| {
        c.stale_after = Duration::from_millis(200);
    });
    let dst = Arc::new(Mutex::new(dest.addr));
    let f = {
        let dst = dst.clone();
        fixture_mesh_dst(
            Arc::new(move |_: &_| Ok(*dst.lock().unwrap())),
            &state,
            None,
        )
        .await
    };
    // Outside the view: the byte relay.
    let mut client = H1Client::connect(f.addr).await;
    for _ in 0..3 {
        let (head, body) = client.get("/x").await;
        assert!(from(&head, &body, "dest"), "{head}");
    }
    assert_eq!(faults(&f, "abort"), 0.0);
    *dst.lock().unwrap() = cluster.addr;
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/x").await;
    assert!(aborted(&head, 503), "{head}");
    assert_eq!(faults(&f, "abort"), 1.0);
    // A stale view: the next request of the open connection, and a new connection.
    tokio::time::sleep(Duration::from_millis(400)).await;
    state.tick();
    let (head, body) = client.get("/x").await;
    assert!(from(&head, &body, "cluster"), "{head}");
    let (head, body) = H1Client::connect(f.addr).await.get("/x").await;
    assert!(from(&head, &body, "cluster"), "{head}");
    assert_eq!(faults(&f, "abort"), 1.0);
    // `"mesh": false`.
    state.hold(view_text(&[], 0, false), "\"2-a\"");
    let (head, body) = H1Client::connect(f.addr).await.get("/x").await;
    assert!(from(&head, &body, "cluster"), "{head}");
    assert_eq!(faults(&f, "abort"), 1.0);
    assert_eq!(pod.counts.requests(), 0);
}

#[tokio::test]
async fn flt_23_upgrade_and_connect_reach_their_destination() {
    let _logs = capture_logs();
    let own = RawPod::start(|head| {
        if head.starts_with(b"GET /ws") {
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nx-from: own\r\n\r\n".to_vec()
        } else if head.starts_with(b"CONNECT") {
            b"HTTP/1.1 200 OK\r\nx-from: own\r\n\r\n".to_vec()
        } else {
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".to_vec()
        }
    })
    .await;
    let cluster = RawPod::start(|_| {
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nx-from: cluster\r\n\r\n".to_vec()
    })
    .await;
    let state = mesh_state(
        &[cart(cluster.addr.port(), &[own.addr]).faults(ALL)],
        |_| {},
    );
    let f = fixture_mesh(cluster.addr, &state, None).await;
    // ①: an `Upgrade` after a request.
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/x").await;
    assert!(aborted(&head, 503), "{head}");
    client.send(UPGRADE).await;
    let (head, _) = client.read().await.expect("the 101");
    assert!(
        head.starts_with("HTTP/1.1 101") && head.contains("x-from: own"),
        "{head}"
    );
    // ②: a `CONNECT` after a request.
    let mut client = H1Client::connect(f.addr).await;
    let (head, _) = client.get("/x").await;
    assert!(aborted(&head, 503), "{head}");
    client
        .send(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("the 200");
    assert!(
        head.starts_with("HTTP/1.1 200") && head.contains("x-from: own"),
        "{head}"
    );
    // ③: an `Upgrade` as the first request.
    let mut client = H1Client::connect(f.addr).await;
    client.send(UPGRADE).await;
    let (head, _) = client.read().await.expect("the 101");
    assert!(
        head.starts_with("HTTP/1.1 101") && head.contains("x-from: cluster"),
        "{head}"
    );
    let received = heads(&own.received());
    assert!(
        received.iter().any(|h| h.starts_with("get /ws")),
        "{received:?}"
    );
    assert!(
        received
            .iter()
            .any(|h| h.starts_with("connect example.com:443")),
        "{received:?}"
    );
    assert!(
        !received.iter().any(|h| h.starts_with("get /x")),
        "{received:?}"
    );
    assert_eq!(faults(&f, "abort"), 2.0);
}

#[tokio::test]
async fn flt_26_a_view_adding_faults_applies_from_the_next_request() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(80, &[pod.addr])], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    let (head, body) = client.get("/a").await;
    assert!(from(&head, &body, "pod"), "{head}");
    state.hold(
        view_services(&[cart(80, &[pod.addr]).faults(ALL)]),
        "\"2-a\"",
    );
    let (head, _) = client.get("/a").await;
    assert!(aborted(&head, 503), "{head}");
    state.hold(view_services(&[cart(80, &[pod.addr])]), "\"3-a\"");
    let (head, body) = client.get("/a").await;
    assert!(from(&head, &body, "pod"), "{head}");
    assert_eq!(pod.counts.requests(), 2);
    assert_eq!(faults(&f, "abort"), 1.0);
}

#[tokio::test]
async fn flt_26_a_delay_in_flight_finishes_when_the_view_removes_its_fault() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let state = mesh_state(&[cart(80, &[pod.addr]).faults(&delay(100, 500))], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let addr = f.addr;
    let held = tokio::spawn(async move {
        let began = Instant::now();
        let (head, body) = H1Client::connect(addr).await.get("/held").await;
        (head, body, began.elapsed())
    });
    until("the delay is counted", || faults(&f, "delay") == 1.0).await;
    state.hold(view_services(&[cart(80, &[pod.addr])]), "\"2-a\"");
    let (head, body, took) = held.await.unwrap();
    assert!(from(&head, &body, "pod"), "{head}");
    assert!(took >= Duration::from_millis(500), "{took:?}");
    let (head, body) = H1Client::connect(f.addr).await.get("/next").await;
    assert!(from(&head, &body, "pod"), "{head}");
    let lines = logs.access_exactly(2).await;
    let line = |path: &str| {
        lines
            .iter()
            .find(|l| l["path"] == path)
            .unwrap_or_else(|| panic!("no line of {path}"))
            .clone()
    };
    let held = line("/held");
    assert_eq!(held["fault_delay_ms"], 500, "{held}");
    let next = line("/next");
    assert!(absent(&next, "fault"), "{next}");
    assert_eq!(pod.counts.requests(), 2);
}

#[tokio::test]
async fn flt_26_one_series_per_service_and_kind_evicted_5_minutes_after_the_service_left() {
    let _logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let ip = |i: usize| format!("10.{}.{}.{}", 96 + i / 60_000, (i / 250) % 240, i % 250 + 1);
    let list = format!(
        r#"[{},{{"delay":{{"fixedMs":1,"percent":100}}}}]"#,
        abort_on("/a", 100, 503)
    );
    let services: Vec<Svc> = (0..300)
        .map(|i| Svc::new(leak(format!("shop/s{i}")), leak(ip(i)), 80, &[pod.addr]).faults(&list))
        .collect();
    let state = mesh_state(&services, |_| {});
    let next = Arc::new(Mutex::new(0usize));
    let addrs: Vec<SocketAddr> = services.iter().map(|s| s.dst(80)).collect();
    let dst = {
        let next = next.clone();
        Arc::new(move |_: &_| Ok(addrs[*next.lock().unwrap()]))
    };
    let f = fixture_mesh_dst(dst, &state, None).await;
    let series = |f: &Fixture| -> Vec<String> {
        f.metrics
            .encode()
            .lines()
            .filter(|l| l.starts_with("mimic_mesh_faults_total{"))
            .map(str::to_string)
            .collect()
    };
    assert!(no_series(&f.metrics.encode(), "mimic_mesh_faults_total{"));
    for i in 0..300 {
        *next.lock().unwrap() = i;
        let mut client = H1Client::connect(f.addr).await;
        let (head, _) = client.get(if i % 2 == 0 { "/a" } else { "/d" }).await;
        let want = if i % 2 == 0 { 503 } else { 200 };
        assert_eq!(status(&head), want, "service {i}: {head}");
    }
    let lines = series(&f);
    assert_eq!(lines.len(), 300, "{lines:?}");
    for i in 0..300 {
        let kind = if i % 2 == 0 { "abort" } else { "delay" };
        let want = format!("mimic_mesh_faults_total{{service=\"shop/s{i}\",kind=\"{kind}\"}} 1");
        assert_eq!(
            lines.iter().filter(|l| **l == want).count(),
            1,
            "{want}: {lines:?}"
        );
    }
    // Every Service leaves the view.
    state.hold(view_services(&[]), "\"2-a\"");
    let now = Instant::now();
    state.evict(now + Duration::from_secs(200));
    assert_eq!(series(&f).len(), 300, "kept 200 s");
    state.evict(now + Duration::from_secs(301));
    assert_eq!(series(&f), Vec::<String>::new());
}

#[tokio::test]
async fn flt_26_no_label_or_field_carries_request_data() {
    let logs = capture_logs();
    let pod = Pod::http1("pod", 0).await;
    let list = r#"[{"abort":{"percent":100,"status":503},"match":{"headers":[{"name":"x-canary","value":"flt26-header-canary"}],"path":{"type":"Exact","value":"/flt26-path-canary"},"queryParams":[{"name":"q","value":"flt26-query-canary"}]}},{"delay":{"fixedMs":1,"percent":100},"match":{"headers":[{"name":"x-canary","value":"flt26-header-canary"}]}}]"#;
    let state = mesh_state(&[cart(80, &[pod.addr]).faults(list)], |_| {});
    let f = fixture_mesh(cluster_ip(), &state, None).await;
    let mut client = H1Client::connect(f.addr).await;
    client
        .send(b"GET /flt26-path-canary?q=flt26-query-canary HTTP/1.1\r\nHost: cart.shop.svc:80\r\nx-canary: flt26-header-canary\r\n\r\n")
        .await;
    let (head, _) = client.read().await.expect("an answer");
    assert!(aborted(&head, 503), "the abort");
    client
        .send(b"POST /other?q=flt26-query-canary HTTP/1.1\r\nHost: cart.shop.svc:80\r\nx-canary: flt26-header-canary\r\ncontent-length: 17\r\n\r\nflt26-body-canary")
        .await;
    let (head, body) = client.read().await.expect("an answer");
    assert!(
        from(&head, &String::from_utf8_lossy(&body), "pod"),
        "the delay"
    );
    assert_eq!(pod.counts.requests(), 1);
    assert_eq!(faults(&f, "abort"), 1.0);
    assert_eq!(faults(&f, "delay"), 1.0);
    let canaries = [
        "flt26-path-canary",
        "flt26-query-canary",
        "flt26-header-canary",
        "flt26-body-canary",
    ];
    let carries = |text: &str| canaries.iter().any(|c| text.contains(c));
    let scrape = f.metrics.encode();
    assert_eq!(
        scrape.lines().filter(|l| carries(l)).count(),
        0,
        "a series carries a request value"
    );
    logs.access_exactly(2).await;
    let mut carrying = 0;
    for line in logs.text().lines() {
        let mut value: serde_json::Value = serde_json::from_str(line).expect("every line is JSON");
        // The access line's `path` is the only field that holds the path (spec §4.1).
        if value["target"] == "access" {
            value.as_object_mut().unwrap().remove("path");
        }
        if carries(&value.to_string()) {
            carrying += 1;
        }
    }
    assert_eq!(carrying, 0, "a log line carries a request value");
}
