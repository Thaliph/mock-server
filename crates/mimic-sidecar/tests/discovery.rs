//! DSC-12 (HTTP/1), DSC-14, DSC-16, DSC-18, DSC-19: the capture tap, the recorder, the
//! `/discovery` endpoint and the configuration of discovery in the sidecar.

mod common;

use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener as StdListener, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use common::{capture_logs, counted, fixture_discovery};
use mimic_core::discovery::report::{Direction as CoreDirection, Report};
use mimic_runtime::testing::{Process, http_get, run, wait_status};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const TIMEOUT: Duration = Duration::from_secs(10);

/// Reads one HTTP/1 message (head, then `content-length` bytes) from `s`, keeping what
/// follows in `pending`.
async fn read_message(s: &mut TcpStream, pending: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&pending[..p]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if pending.len() >= p + 4 + length {
                let body = pending[p + 4..p + 4 + length].to_vec();
                pending.drain(..p + 4 + length);
                return Some((head, body));
            }
        }
        let mut buf = vec![0u8; 64 * 1024];
        match s.read(&mut buf).await {
            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
            _ => return None,
        }
    }
}

fn response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A small API: `GET /users/<n>` is a 200, `POST /orders` a 201 (a 400 when the body says
/// `bad`), another method on `/orders` a 200, anything else a 404.
async fn app() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                while let Some((head, body)) = read_message(&mut s, &mut pending).await {
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let reply = if let Some(n) = line
                        .strip_prefix("GET /users/")
                        .and_then(|r| r.split(' ').next())
                    {
                        response(
                            "200 OK",
                            &format!("{{\"id\":{n},\"name\":\"user-{n}\",\"active\":true}}"),
                        )
                    } else if line.starts_with("POST /orders") {
                        if body.windows(3).any(|w| w == b"bad") {
                            response("400 Bad Request", "{\"error\":\"bad sku\"}")
                        } else {
                            response("201 Created", "{\"id\":7,\"total\":12.5}")
                        }
                    } else if line.contains(" /orders") {
                        response("200 OK", "{}")
                    } else {
                        response("404 Not Found", "{}")
                    };
                    if s.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// Sends `request` on a new connection and returns the head and body of the answer.
async fn call(addr: SocketAddr, request: &[u8]) -> (String, Vec<u8>) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(request).await.unwrap();
    let mut pending = Vec::new();
    tokio::time::timeout(TIMEOUT, read_message(&mut c, &mut pending))
        .await
        .expect("answered in time")
        .expect("a response")
}

fn start(config: &DiscoveryConfig) -> Discovery {
    Discovery::start(config, Some("pod-a".into())).unwrap()
}

/// The sum of the series of `metric` whose line contains every one of `labels`.
fn sum(text: &str, metric: &str, labels: &[&str]) -> u64 {
    text.lines()
        .filter(|l| l.starts_with(metric) && labels.iter().all(|x| l.contains(x)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

fn templates(report: &Report) -> Vec<String> {
    report
        .endpoints()
        .iter()
        .map(|e| format!("{:?} {} {} {}", e.direction, e.port, e.method, e.template))
        .collect()
}

#[tokio::test]
async fn dsc_12_inbound_http1_exchanges_appear_in_the_report() {
    let app = app().await;
    let discovery = start(&DiscoveryConfig::default());
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    for n in 1..=5 {
        let host = if n == 5 {
            "10.0.0.7:8080"
        } else {
            "svc.ns:8080"
        };
        let (head, _) = call(
            f.addr,
            format!("GET /users/{n} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }
    let order = "{\"sku\":\"a1\",\"qty\":2}";
    let post = |query: &str, body: &str| {
        format!(
            "POST /orders{query} HTTP/1.1\r\nHost: svc.ns:8080\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let (head, _) = call(f.addr, post("?dry_run=true", order).as_bytes()).await;
    assert!(head.starts_with("HTTP/1.1 201"), "{head}");
    let (head, _) = call(f.addr, post("", "{\"sku\":\"bad\",\"qty\":0}").as_bytes()).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let (head, _) = call(
        f.addr,
        b"GET /unknown HTTP/1.1\r\nHost: svc.ns:8080\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    // An exchange the codec cannot frame is not discovered.
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /broken HTTP/1.1\r\nBad Header\r\n\r\n")
        .await
        .unwrap();
    drop(c);
    // An outbound exchange through the same discovery is not discovered either.
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    call(
        out.addr,
        b"GET /users/9 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;

    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 7",
    )
    .await;
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"not_found\"} 1",
    )
    .await;
    let report = discovery.report();
    let port = app.port();
    assert_eq!(
        templates(&report),
        vec![
            format!("Inbound {port} POST /orders"),
            format!("Inbound {port} GET /users/{{users_id}}"),
        ]
    );
    let endpoints = report.endpoints();
    let users = endpoints.iter().find(|e| e.method == "GET").unwrap();
    assert_eq!(users.direction, CoreDirection::Inbound);
    assert_eq!(users.stats.statuses.get(&200), Some(&5));
    assert_eq!(
        users.stats.hosts.iter().cloned().collect::<Vec<_>>(),
        vec!["10.0.0.7:8080".to_string(), "svc.ns:8080".to_string()]
    );
    let orders = endpoints.iter().find(|e| e.method == "POST").unwrap();
    assert_eq!(orders.stats.statuses.get(&201), Some(&1));
    assert_eq!(orders.stats.statuses.get(&400), Some(&1));
    let json = report.to_json();
    for expected in ["dry_run", "qty", "sku", "total", "/users/3", "accept"] {
        assert!(json.contains(expected), "{expected} missing from {json}");
    }
    for absent in ["/broken", "/unknown", "outbound"] {
        assert!(!json.contains(absent), "{absent} in {json}");
    }
    let text = f.metrics.encode();
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_bodies_total",
            &["part=\"response\"", "result=\"parsed\""]
        ),
        7
    );
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_bodies_total",
            &["part=\"request\"", "result=\"parsed\""]
        ),
        2
    );
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_bodies_total",
            &["part=\"request\"", "result=\"empty\""]
        ),
        5
    );
}

fn admin_request(peer: &str) -> Request<Body> {
    let mut request = Request::get("/discovery").body(Body::empty()).unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

#[tokio::test]
async fn dsc_14_discovery_is_answered_to_loopback_peers_only() {
    let discovery = start(&DiscoveryConfig::default());
    let router = discovery.router();
    for peer in ["127.0.0.1:4000", "[::1]:4000", "[::ffff:127.0.0.1]:4000"] {
        let response = router.clone().oneshot(admin_request(peer)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{peer}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "{peer}"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let report = Report::from_json(std::str::from_utf8(&body).unwrap()).unwrap();
        assert_eq!(report.exchanges(), 0);
    }
    for peer in ["10.1.2.3:4000", "[fd00::1]:4000", "[::ffff:10.1.2.3]:4000"] {
        let response = router.clone().oneshot(admin_request(peer)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{peer}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains("version"),
            "{peer}"
        );
    }
}

#[test]
fn dsc_14_binary_answers_from_127_0_0_1_and_ipv6_loopback_and_refuses_the_pod_address() {
    let ipv6 = StdListener::bind("[::1]:0").is_ok();
    let admin = if ipv6 { "[::]:0" } else { "0.0.0.0:0" };
    let mut sidecar = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", admin),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ],
    );
    let bound = sidecar.admin_addr();
    wait_status(
        SocketAddr::from(([127, 0, 0, 1], bound.port())),
        "/readyz",
        200,
        TIMEOUT,
    );
    let (status, body) = http_get(
        SocketAddr::from(([127, 0, 0, 1], bound.port())),
        "/discovery",
    );
    assert_eq!(status, 200, "{body}");
    assert!(Report::from_json(&body).is_ok(), "{body}");
    if ipv6 {
        let (status, body) = http_get(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), bound.port()),
            "/discovery",
        );
        assert_eq!(status, 200, "{body}");
    } else {
        eprintln!("no IPv6 loopback: ::1 not tested");
    }
    // The pod address: the address a route to a public destination would leave from.
    let pod = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_loopback());
    match pod {
        Some(ip) => {
            let (status, body) = http_get(SocketAddr::new(ip, bound.port()), "/discovery");
            assert_eq!(status, 403, "{body}");
            assert!(!body.contains("version"), "{body}");
            let (status, _) = http_get(SocketAddr::new(ip, bound.port()), "/healthz");
            assert_eq!(status, 200);
        }
        None => eprintln!("no non-loopback address: 403 covered by the in-process test"),
    }
}

#[test]
fn dsc_19_invalid_discovery_values_exit_2_before_listening() {
    let names_65 = (0..65)
        .map(|i| format!("n{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let long_name = "a".repeat(65);
    let cases: Vec<(&str, String)> = vec![
        ("MIMIC_DISCOVERY_MAX_ENDPOINTS", "0".into()),
        ("MIMIC_DISCOVERY_MAX_ENDPOINTS", "10001".into()),
        ("MIMIC_DISCOVERY_MAX_ENDPOINTS", "many".into()),
        ("MIMIC_DISCOVERY_MAX_LITERALS", "1".into()),
        ("MIMIC_DISCOVERY_MAX_LITERALS", "1001".into()),
        ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "512Ki".into()),
        ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "2Gi".into()),
        ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "16MB".into()),
        ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "Mi".into()),
        ("MIMIC_DISCOVERY_CAPTURE_BUDGET", "1023Ki".into()),
        ("MIMIC_DISCOVERY_CAPTURE_BUDGET", "8 Mi".into()),
        ("MIMIC_DISCOVERY_QUEUE", "0".into()),
        ("MIMIC_DISCOVERY_QUEUE", "65537".into()),
        ("MIMIC_DISCOVERY_REDACT_NAMES", names_65),
        ("MIMIC_DISCOVERY_REDACT_NAMES", "ok,Bad Name".into()),
        ("MIMIC_DISCOVERY_REDACT_NAMES", "ok,,again".into()),
        ("MIMIC_DISCOVERY_REDACT_NAMES", long_name),
    ];
    for (var, value) in &cases {
        let (status, stdout, stderr) = run(BIN, &[], &[(var, value.as_str())]);
        assert_eq!(status.code(), Some(2), "{var}={value}: {stderr}");
        assert!(stderr.contains(var), "{var}={value}: {stderr}");
        assert!(!stdout.contains("listening"), "{var}={value}: {stdout}");
    }
}

#[test]
fn dsc_19_valid_discovery_values_are_accepted() {
    let mut sidecar = Process::spawn(
        BIN,
        &[
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
            ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
            ("MIMIC_DISCOVERY_MAX_ENDPOINTS", "10000"),
            ("MIMIC_DISCOVERY_MAX_LITERALS", "2"),
            ("MIMIC_DISCOVERY_SAMPLE_BUDGET", "1Mi"),
            ("MIMIC_DISCOVERY_CAPTURE_BUDGET", "1024Mi"),
            ("MIMIC_DISCOVERY_QUEUE", "65536"),
            ("MIMIC_DISCOVERY_REDACT_NAMES", "x-tenant-key,pin"),
        ],
    );
    let addr = sidecar.admin_addr();
    wait_status(addr, "/readyz", 200, TIMEOUT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dsc_16_discovery_never_slows_or_alters_traffic() {
    mimic_sidecar::listener::raise_fd_limit();
    let reply = response("200 OK", &format!("{{\"data\":\"{}\"}}", "y".repeat(500)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = listener.local_addr().unwrap();
    let canned = reply.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let canned = canned.clone();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                while let Some((_, body)) = read_message(&mut s, &mut pending).await {
                    // The body arrived unchanged.
                    let intact = body.len() == 200 * 1024 && body.iter().all(|&b| b == b'z');
                    let out = if intact {
                        canned.clone()
                    } else {
                        response("500 Internal Server Error", "{}")
                    };
                    if s.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let config = DiscoveryConfig {
        queue: 1,
        capture_budget: 1024 * 1024,
        ..DiscoveryConfig::default()
    };
    let discovery = start(&config);
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let request = {
        let mut r = b"POST /upload HTTP/1.1\r\nHost: a\r\nContent-Type: application/json\r\nContent-Length: 204800\r\nConnection: close\r\n\r\n".to_vec();
        r.extend(vec![b'z'; 200 * 1024]);
        Arc::new(r)
    };
    // 2000 requests, 100 connections at a time: with 500 at once the plain proxy already
    // resets connections on this test host, whatever discovery does.
    let permits = Arc::new(tokio::sync::Semaphore::new(100));
    let mut tasks = Vec::new();
    for _ in 0..2000 {
        let (addr, request, reply) = (f.addr, request.clone(), reply.clone());
        let permit = permits.clone().acquire_owned().await.unwrap();
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&request).await.unwrap();
            let mut got = vec![0u8; reply.len()];
            tokio::time::timeout(Duration::from_secs(60), c.read_exact(&mut got))
                .await
                .expect("answered in time")
                .unwrap();
            got == reply
        }));
    }
    for task in tasks {
        assert!(
            task.await.unwrap(),
            "a response differs from what the app sent"
        );
    }
    // Every exchange has exactly one result.
    for _ in 0..200 {
        if sum(&f.metrics.encode(), "mimic_discovery_exchanges_total", &[]) == 2000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = f.metrics.encode();
    assert_eq!(
        sum(&text, "mimic_discovery_exchanges_total", &[]),
        2000,
        "{text}"
    );
    assert!(
        sum(
            &text,
            "mimic_discovery_exchanges_total",
            &["result=\"dropped\""]
        ) > 0,
        "{text}"
    );
    assert!(
        sum(
            &text,
            "mimic_discovery_bodies_total",
            &["result=\"budget\""]
        ) > 0,
        "{text}"
    );
}

fn log_lines(logs: &common::Logs) -> Vec<serde_json::Value> {
    logs.text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["target"] == "discovery")
        .collect()
}

#[tokio::test]
async fn dsc_18_logs_name_endpoints_and_absorbed_templates_without_values() {
    let logs = capture_logs();
    let app = app().await;
    let discovery = start(&DiscoveryConfig::default());
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let get = |path: &str, extra: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: a\r\n{extra}Connection: close\r\n\r\n")
    };
    call(f.addr, get("/users/1?token=CANARYQUERY&page=2", "Authorization: Bearer CANARYHEADER\r\nCookie: sid=CANARYCOOKIE\r\nX-Custom: ghp_CANARYPREFIX\r\n").as_bytes()).await;
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    let lines = log_lines(&logs);
    let discovered: Vec<_> = lines
        .iter()
        .filter(|l| l["message"] == "endpoint discovered")
        .collect();
    assert_eq!(discovered.len(), 1, "{lines:?}");
    let line = discovered[0];
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["direction"], "inbound");
    assert_eq!(line["port"], app.port());
    assert_eq!(line["method"], "GET");
    assert_eq!(line["template"], "/users/{users_id}");
    for canary in ["CANARY", "token=", "Bearer", "sid="] {
        assert!(!logs.text().contains(canary), "{canary} in {}", logs.text());
    }
    // Redactions are counted by location and rule.
    let text = f.metrics.encode();
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_redactions_total",
            &["location=\"query\"", "rule=\"name\""]
        ),
        1
    );
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_redactions_total",
            &["location=\"header\"", "rule=\"name\""]
        ),
        2
    );
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_redactions_total",
            &["location=\"header\"", "rule=\"token_prefix\""]
        ),
        1
    );
    assert_eq!(
        sum(
            &text,
            "mimic_discovery_endpoints",
            &["direction=\"inbound\""]
        ),
        1
    );
}

#[tokio::test]
async fn dsc_18_absorption_sample_budget_and_endpoint_limit_are_logged() {
    let logs = capture_logs();
    let app = app().await;
    let config = DiscoveryConfig {
        max_literals: 3,
        max_endpoints: 5,
        sample_budget: 1024 * 1024,
        ..DiscoveryConfig::default()
    };
    let discovery = start(&config);
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    // The app answers `/users/<n>` with a 200: four literals under /users absorb into one parameter.
    let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n");
    for name in ["ann", "bob", "cyd", "dee"] {
        call(f.addr, get(&format!("/users/{name}")).as_bytes()).await;
    }
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 4",
    )
    .await;
    let lines = log_lines(&logs);
    let absorbed: Vec<_> = lines
        .iter()
        .filter(|l| l["message"] == "template absorbed")
        .collect();
    assert_eq!(absorbed.len(), 1, "{lines:?}");
    assert_eq!(absorbed[0]["level"], "INFO");
    assert_eq!(absorbed[0]["template"], "/users/{users_id}");
    let old = absorbed[0]["old"].as_str().unwrap();
    for literal in ["/users/ann", "/users/bob", "/users/cyd"] {
        assert!(old.contains(literal), "{old}");
    }
    assert_eq!(discovery.report().endpoints().len(), 1);
    assert_eq!(
        lines
            .iter()
            .filter(|l| l["message"] == "endpoint discovered")
            .count(),
        3,
        "{lines:?}"
    );
    assert_eq!(
        sum(
            &f.metrics.encode(),
            "mimic_discovery_endpoints",
            &["direction=\"inbound\""]
        ),
        1
    );

    // New methods on /orders fill the endpoint limit.
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let request = format!(
            "{method} /orders HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        call(f.addr, request.as_bytes()).await;
    }
    call(
        f.addr,
        b"OPTIONS /orders HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"endpoint_limit\"}",
    )
    .await;
    let lines = log_lines(&logs);
    let limit: Vec<_> = lines
        .iter()
        .filter(|l| l["message"] == "endpoint limit reached")
        .collect();
    assert_eq!(limit.len(), 1, "{lines:?}");
    assert_eq!(limit[0]["level"], "WARN");

    // A sample larger than the budget is rejected once, with one warning.
    for i in 0..30 {
        let body = format!("{{\"blob\":\"{i}{}\"}}", "q".repeat(100 * 1024));
        let request = format!(
            "POST /orders HTTP/1.1\r\nHost: a\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        call(f.addr, request.as_bytes()).await;
    }
    counted(
        &f.metrics,
        "mimic_discovery_samples_total{direction=\"inbound\",result=\"budget\"}",
    )
    .await;
    let lines = log_lines(&logs);
    let full: Vec<_> = lines
        .iter()
        .filter(|l| l["message"] == "sample budget full")
        .collect();
    assert_eq!(full.len(), 1, "{lines:?}");
    assert_eq!(full[0]["level"], "WARN");
    assert!(sum(&f.metrics.encode(), "mimic_discovery_sample_bytes", &[]) <= 1024 * 1024);
    for canary in ["qqqq", "blob"] {
        assert!(!logs.text().contains(canary), "{canary} in the logs");
    }
}
