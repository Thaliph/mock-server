//! DSC-12 (h2 part) and DSC-15: the `h2` tee of the discovery tap, and nothing sensitive
//! leaving the sidecar through `/discovery`, the logs or the metrics.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{capture_logs, counted, fixture_discovery};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::discovery::report::Direction as CoreDirection;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);

type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// An `h2c` app: `GET /users/<n>` is a 200, `POST /orders` a 201 (a 400 when the body says
/// `bad`), anything else a 404. Every request it gets is kept as (head, body).
async fn app() -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            let kept = kept.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let kept = kept.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body = body.collect().await.unwrap().to_bytes();
                        let mut head = format!("{} {}", parts.method, parts.uri);
                        let mut headers: Vec<_> = parts
                            .headers
                            .iter()
                            .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap()))
                            .collect();
                        headers.sort();
                        for h in headers {
                            head.push('\n');
                            head.push_str(&h);
                        }
                        kept.lock().unwrap().push((head, body.to_vec()));
                        let path = parts.uri.path();
                        let (status, json) = if let Some(n) = path.strip_prefix("/users/") {
                            (
                                200,
                                format!(
                                    "{{\"id\":{n:?},\"name\":\"user\",\"password\":\"CANARY-RESP-PW\"}}"
                                ),
                            )
                        } else if path == "/orders" {
                            if body.windows(3).any(|w| w == b"bad") {
                                (400, "{\"error\":\"bad sku\"}".to_string())
                            } else {
                                (201, "{\"id\":7,\"total\":12.5}".to_string())
                            }
                        } else {
                            (404, "{}".to_string())
                        };
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .header("set-cookie", "sid=CANARY-SETCOOKIE; Path=/")
                                .body(Full::new(Bytes::from(json)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .auto_date_header(false)
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    (addr, seen)
}

async fn client(proxy: SocketAddr) -> SendRequest<Full<Bytes>> {
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

async fn send(sender: &mut SendRequest<Full<Bytes>>, req: Request<Full<Bytes>>) -> (u16, Vec<u8>) {
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .expect("answered")
        .unwrap();
    let status = response.status().as_u16();
    (
        status,
        response.collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn sum(text: &str, metric: &str, labels: &[&str]) -> u64 {
    text.lines()
        .filter(|l| l.starts_with(metric) && labels.iter().all(|x| l.contains(x)))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

#[tokio::test]
async fn dsc_12_inbound_h2_exchanges_appear_in_the_report() {
    // Every test of this file has a subscriber: callsites are registered once per process.
    let _logs = capture_logs();
    let (app, _) = app().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    for n in 1..=5 {
        let host = if n == 5 {
            "10.0.0.7:8080"
        } else {
            "svc.ns:8080"
        };
        let req = Request::get(format!("http://{host}/users/{n}"))
            .header("accept", "application/json")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(send(&mut sender, req).await.0, 200);
    }
    let post = |query: &str, body: &'static str| {
        Request::post(format!("http://svc.ns:8080/orders{query}"))
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap()
    };
    let order = "{\"sku\":\"a1\",\"qty\":2}";
    assert_eq!(send(&mut sender, post("?dry_run=true", order)).await.0, 201);
    assert_eq!(
        send(&mut sender, post("", "{\"sku\":\"bad\",\"qty\":0}"))
            .await
            .0,
        400
    );
    let req = Request::get("http://svc.ns:8080/unknown")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(send(&mut sender, req).await.0, 404);
    // An outbound exchange through the same discovery is not discovered.
    let out = fixture_discovery(Direction::Outbound, &discovery, move |_| Some(app)).await;
    let mut out_sender = client(out.addr).await;
    let req = Request::get("http://x/users/9")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(send(&mut out_sender, req).await.0, 200);

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
    let endpoints = report.endpoints();
    let templates: Vec<_> = endpoints
        .iter()
        .map(|e| format!("{} {} {}", e.port, e.method, e.template))
        .collect();
    assert_eq!(
        templates,
        vec![
            format!("{port} POST /orders"),
            format!("{port} GET /users/{{users_id}}")
        ]
    );
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
    for absent in ["/unknown", "outbound"] {
        assert!(!json.contains(absent), "{absent} in {json}");
    }
    let text = f.metrics.encode();
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
            &["part=\"response\"", "result=\"parsed\""]
        ),
        7
    );
}

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJDQU5BUllKV1QifQ.c2lnbmF0dXJlQ0FOQVJZ";
const CANARIES: [&str; 11] = [
    "CANARY-BEARER",
    "CANARY-COOKIE",
    "CANARY-APIKEY",
    "CANARY-QUERY",
    "eyJzdWIiOiJDQU5BUllKV1QifQ",
    "4111111111111111",
    "CANARY-REQ-PW",
    "CANARY-RESP-PW",
    "CANARY-SETCOOKIE",
    "ghp_CANARYPREFIX",
    "c2lnbmF0dXJlQ0FOQVJZ",
];

fn body_json() -> String {
    "{\"user\":\"ann\",\"password\":\"CANARY-REQ-PW\",\"card\":\"4111111111111111\"}".to_string()
}

fn assert_no_leak(what: &str, text: &str) {
    for canary in CANARIES {
        assert!(!text.contains(canary), "{canary} in {what}");
    }
}

#[tokio::test]
async fn dsc_15_no_leak_end_to_end_over_h2() {
    let logs = capture_logs();
    let (app, seen) = app().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    let body = body_json();
    let req = Request::post(format!(
        "http://svc.ns/users/{JWT}?token=CANARY-QUERY&page=1"
    ))
    .header("authorization", "Bearer CANARY-BEARER")
    .header("cookie", "sid=CANARY-COOKIE; theme=dark")
    .header("x-api-key", "CANARY-APIKEY")
    .header("x-custom", "ghp_CANARYPREFIX")
    .header("content-type", "application/json")
    .body(Full::new(Bytes::from(body.clone())))
    .unwrap();
    let (status, answer) = send(&mut sender, req).await;
    assert_eq!(status, 200);
    // The client gets what the app sent.
    assert_eq!(
        String::from_utf8(answer).unwrap(),
        format!("{{\"id\":{JWT:?},\"name\":\"user\",\"password\":\"CANARY-RESP-PW\"}}")
    );
    // A route the app answers 200 with a password in the response body.
    let req = Request::get("http://svc.ns/users/42?token=CANARY-QUERY")
        .header("authorization", "Bearer CANARY-BEARER")
        .header("cookie", "sid=CANARY-COOKIE")
        .header("x-api-key", "CANARY-APIKEY")
        .header("x-custom", "ghp_CANARYPREFIX")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let (status, answer) = send(&mut sender, req).await;
    assert_eq!(status, 200);
    assert!(
        String::from_utf8(answer)
            .unwrap()
            .contains("CANARY-RESP-PW")
    );
    let req = Request::get(format!("http://svc.ns/users/{JWT}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(send(&mut sender, req).await.0, 200);
    let req = Request::post("http://svc.ns/orders?token=CANARY-QUERY")
        .header("authorization", "Bearer CANARY-BEARER")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap();
    assert_eq!(send(&mut sender, req).await.0, 201);

    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 4",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = seen.lock().unwrap().clone();
    // Bytes reach the app unchanged.
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].1, body.as_bytes());
    assert!(seen[0].0.contains("authorization: Bearer CANARY-BEARER"));
    assert!(seen[0].0.contains("x-api-key: CANARY-APIKEY"));
    assert!(seen[0].0.contains("token=CANARY-QUERY"));
    let json = discovery.report().to_json();
    assert!(json.contains("<redacted>"), "{json}");
    assert_no_leak("/discovery", &json);
    assert_no_leak("the metrics", &f.metrics.encode());
    // The access lines go through the log queue.
    let mut paths = Vec::new();
    for _ in 0..100 {
        paths = logs
            .access()
            .iter()
            .filter_map(|l| l["path"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        if paths.len() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(paths.iter().any(|p| p == "/users/<redacted>"), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("eyJ")), "{paths:?}");
    assert_no_leak("the logs", &logs.text());
}
