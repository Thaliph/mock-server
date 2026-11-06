//! EGR-09 (cleartext outbound HTTP/1, h2 and gRPC), EGR-17 (the recording part: no canary in
//! `/discovery`, the logs or the metrics) and EGR-18 (`MIMIC_DISCOVERY_MAX_AUTHORITIES`).

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request as AxumRequest, StatusCode};
use common::{capture_logs, counted, fixture_discovery};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::discovery::report::{Direction as CoreDirection, Report};
use mimic_runtime::config::Env;
use mimic_runtime::testing::run;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");
const TIMEOUT: Duration = Duration::from_secs(10);
const AUTHORITY: &str = "api.example.test:8080";

// Secrets the client sends to a provider. None may leave the sidecar.
const BEARER: &str = "CANARY-BEARER-91c";
const COOKIE: &str = "CANARY-COOKIE-91c";
const API_KEY: &str = "CANARY-APIKEY-91c";
const SUBSCRIPTION: &str = "CANARY-SUBSCRIPTION-91c";
const CUSTOM: &str = "CANARY-CUSTOM-91c";
const QUERY_KEY: &str = "CANARY-QKEY-91c";
const QUERY_SIG: &str = "CANARY-QSIG-91c";
const IDEMPOTENCY: &str = "CANARY-IDEM-91c";
const GOOGLE: &str = "AIzaCANARY0123456789abcdefghijklmnopqrs";
const CANARIES: [&str; 9] = [
    BEARER,
    COOKIE,
    API_KEY,
    SUBSCRIPTION,
    CUSTOM,
    QUERY_KEY,
    QUERY_SIG,
    IDEMPOTENCY,
    GOOGLE,
];

type Seen = Arc<Mutex<Vec<String>>>;

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

/// The provider: `GET /users/<n>` is a 200, `POST /orders` a 201 (a 400 when the body says
/// `bad`), anything else a 404. Every request it gets is kept, head and body.
async fn provider() -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let kept = kept.clone();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                while let Some((head, body)) = read_message(&mut s, &mut pending).await {
                    kept.lock()
                        .unwrap()
                        .push(format!("{head}\n\n{}", String::from_utf8_lossy(&body)));
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let reply = if let Some(n) = line
                        .strip_prefix("GET /users/")
                        .and_then(|r| r.split(' ').next())
                    {
                        response("200 OK", &format!("{{\"id\":{n},\"name\":\"user-{n}\"}}"))
                    } else if line.starts_with("POST /orders") {
                        if body.windows(3).any(|w| w == b"bad") {
                            response("400 Bad Request", "{\"error\":\"bad sku\"}")
                        } else {
                            response("201 Created", "{\"id\":7,\"total\":12.5}")
                        }
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
    (addr, seen)
}

async fn call(addr: SocketAddr, request: &[u8]) -> (String, Vec<u8>) {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(request).await.unwrap();
    let mut pending = Vec::new();
    tokio::time::timeout(TIMEOUT, read_message(&mut c, &mut pending))
        .await
        .expect("answered in time")
        .expect("a response")
}

fn get(path: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {AUTHORITY}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

fn post(path: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nHost: {AUTHORITY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn outbound(report: &Report) -> Vec<(String, String, String)> {
    report
        .endpoints()
        .iter()
        .filter(|e| e.direction == CoreDirection::Outbound)
        .map(|e| {
            (
                e.authority.unwrap().to_string(),
                e.method.to_string(),
                e.template.clone(),
            )
        })
        .collect()
}

fn scope(report: &Report) -> serde_json::Value {
    let json: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    json["scopes"]["outbound"][AUTHORITY].clone()
}

#[tokio::test]
async fn egr_09_cleartext_http1_outbound_exchanges_appear_under_their_authority() {
    let _logs = capture_logs();
    let (app, seen) = provider().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let inbound = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    for n in 1..=5 {
        let (head, body) = call(out.addr, &get(&format!("/users/{n}"))).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        // The destination's bytes reach the app unchanged.
        assert_eq!(
            String::from_utf8(body).unwrap(),
            format!("{{\"id\":{n},\"name\":\"user-{n}\"}}")
        );
    }
    let order = "{\"sku\":\"a1\",\"qty\":2}";
    let (head, _) = call(out.addr, &post("/orders?dry_run=true", order)).await;
    assert!(head.starts_with("HTTP/1.1 201"), "{head}");
    let (head, _) = call(out.addr, &post("/orders", "{\"sku\":\"bad\"}")).await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let (head, _) = call(out.addr, &get("/missing")).await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    // An exchange the codec cannot frame is not recorded.
    let mut c = TcpStream::connect(out.addr).await.unwrap();
    c.write_all(b"GET /broken HTTP/1.1\r\nBad Header\r\n\r\n")
        .await
        .unwrap();
    drop(c);
    // Inbound exchanges sent meanwhile are recorded as before.
    let (head, _) = call(
        inbound.addr,
        b"GET /users/9 HTTP/1.1\r\nHost: svc.ns:8080\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    counted(
        &out.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 8",
    )
    .await;
    counted(
        &inbound.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    let report = discovery.report();
    assert_eq!(
        outbound(&report),
        [
            (AUTHORITY.into(), "GET".into(), "/missing".into()),
            (AUTHORITY.into(), "POST".into(), "/orders".into()),
            (AUTHORITY.into(), "GET".into(), "/users/{users_id}".into()),
        ]
    );
    let endpoints = report.endpoints();
    let find = |m: &str, t: &str| {
        endpoints
            .iter()
            .find(|e| e.direction == CoreDirection::Outbound && e.method == m && e.template == t)
            .unwrap()
    };
    assert_eq!(find("GET", "/users/{users_id}").stats.statuses[&200], 5);
    let orders = find("POST", "/orders");
    assert_eq!(orders.stats.statuses[&201], 1);
    assert_eq!(orders.stats.statuses[&400], 1);
    assert_eq!(find("GET", "/missing").stats.statuses[&404], 1);
    assert!(find("GET", "/missing").stats.samples().len() == 1);
    assert!(
        endpoints
            .iter()
            .filter(|e| e.direction == CoreDirection::Outbound)
            .all(|e| e.stats.hosts.is_empty())
    );
    let json = report.to_json().unwrap();
    for expected in ["dry_run", "qty", "sku", "total"] {
        assert!(json.contains(expected), "{expected} missing");
    }
    assert!(!json.contains("/broken"));
    let origin = &scope(&report)["origin"];
    assert_eq!(origin["schemes"], serde_json::json!({"http": 8}));
    assert_eq!(
        origin["dst"],
        serde_json::json!([format!("127.0.0.1:{}", app.port())])
    );
    let inbound_templates: Vec<_> = endpoints
        .iter()
        .filter(|e| e.direction == CoreDirection::Inbound)
        .map(|e| e.template.clone())
        .collect();
    assert_eq!(inbound_templates, ["/users/{users_id}"]);
    // Bytes reach the destination unchanged.
    let kept = seen.lock().unwrap().clone();
    assert!(kept.iter().any(|r| r.contains(order)));
}

/// An `h2c` provider: `GET /users/<n>` a 200, a gRPC call a trailers-only `grpc-status: 0`.
async fn h2_provider() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let path = req.uri().path().to_string();
                    let _ = req.into_body().collect().await;
                    let response = if path.starts_with("/pkg.Svc/") {
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/grpc")
                            .header("grpc-status", "0")
                            .body(Full::new(Bytes::from_static(&[0, 0, 0, 0, 0])))
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from_static(
                                b"{\"ok\":true,\"name\":\"u\"}",
                            )))
                            .unwrap()
                    };
                    Ok::<_, Infallible>(response)
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    addr
}

async fn h2_client(proxy: SocketAddr) -> SendRequest<Full<Bytes>> {
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

#[tokio::test]
async fn egr_09_cleartext_h2_and_grpc_outbound_exchanges_are_recorded() {
    let _logs = capture_logs();
    let app = h2_provider().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let mut sender = h2_client(out.addr).await;
    for n in 1..=3 {
        let req = Request::get(format!("http://{AUTHORITY}/users/{n}"))
            .header("accept", "application/json")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
            .await
            .expect("answered")
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"{\"ok\":true,\"name\":\"u\"}");
    }
    let req = Request::post(format!("http://{AUTHORITY}/pkg.Svc/Call"))
        .header("content-type", "application/grpc")
        .body(Full::new(Bytes::from_static(&[0, 0, 0, 0, 0])))
        .unwrap();
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.into_body().collect().await;
    counted(
        &out.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 4",
    )
    .await;
    let report = discovery.report();
    assert_eq!(
        outbound(&report),
        [
            (AUTHORITY.into(), "POST".into(), "/pkg.Svc/Call".into()),
            (AUTHORITY.into(), "GET".into(), "/users/{users_id}".into()),
        ]
    );
    let grpc = report
        .endpoints()
        .into_iter()
        .find(|e| e.template == "/pkg.Svc/Call")
        .unwrap()
        .grpc;
    assert!(grpc);
    let origin = &scope(&report)["origin"];
    assert_eq!(origin["schemes"], serde_json::json!({"http": 4}));
}

#[tokio::test]
async fn egr_17_no_canary_leaves_the_sidecar_from_cleartext_outbound_exchanges() {
    let logs = capture_logs();
    let (app, seen) = provider().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let body = format!(
        "{{\"idempotency_key\":\"{IDEMPOTENCY}\",\"user\":\"ann\",\"password\":\"{CUSTOM}\"}}"
    );
    let request = format!(
        "POST /orders?key={QUERY_KEY}&sig={QUERY_SIG}&maps={GOOGLE}&page=1 HTTP/1.1\r\n\
         Host: {AUTHORITY}\r\n\
         Authorization: Bearer {BEARER}\r\n\
         Cookie: sid={COOKIE}\r\n\
         X-Api-Key: {API_KEY}\r\n\
         Ocp-Apim-Subscription-Key: {SUBSCRIPTION}\r\n\
         X-Custom: {CUSTOM}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (head, _) = call(out.addr, request.as_bytes()).await;
    assert!(head.starts_with("HTTP/1.1 201"), "{head}");
    counted(
        &out.metrics,
        "mimic_discovery_exchanges_total{direction=\"outbound\",result=\"recorded\"} 1",
    )
    .await;
    // The destination got every secret unchanged: nothing is altered.
    let kept = seen.lock().unwrap().clone();
    assert_eq!(kept.len(), 1);
    for canary in CANARIES {
        assert!(kept[0].contains(canary));
    }
    // `/discovery` over the admin router, to a loopback peer.
    let mut request = AxumRequest::get("/discovery").body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let answer = discovery.router().oneshot(request).await.unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    let served = String::from_utf8(
        answer
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(served.contains("<redacted>"));
    // The names stay, the values do not.
    for name in ["x-api-key", "ocp-apim-subscription-key", "x-custom"] {
        assert!(served.contains(name));
    }
    let texts = [
        ("discovery", served),
        ("report", discovery.report().to_json().unwrap()),
        ("metrics", out.metrics.encode()),
    ];
    for (what, text) in &texts {
        for (i, canary) in CANARIES.iter().enumerate() {
            let leaked = text.contains(canary);
            assert!(!leaked, "canary {i} in {what}");
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = logs.text();
    for canary in CANARIES {
        assert!(!log.contains(canary));
    }
}

#[test]
fn egr_18_invalid_max_authorities_exits_2_before_listening() {
    for value in ["0", "10001", "abc"] {
        let var = "MIMIC_DISCOVERY_MAX_AUTHORITIES";
        let (status, stdout, stderr) = run(BIN, &[], &[(var, value)]);
        assert_eq!(status.code(), Some(2), "{value}: {stderr}");
        assert!(stderr.contains(var), "{value}: {stderr}");
        assert!(!stdout.contains("listening"), "{value}: {stdout}");
    }
}

#[test]
fn egr_18_the_default_limit_is_64_and_a_valid_value_is_taken() {
    let env = |pairs: &[(&str, &str)]| {
        Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    };
    let config = DiscoveryConfig::from_env(&env(&[])).unwrap();
    assert_eq!(config.max_authorities, 64);
    assert_eq!(config.limits().max_authorities, 64);
    let config =
        DiscoveryConfig::from_env(&env(&[("MIMIC_DISCOVERY_MAX_AUTHORITIES", "10000")])).unwrap();
    assert_eq!(config.limits().max_authorities, 10_000);
}
