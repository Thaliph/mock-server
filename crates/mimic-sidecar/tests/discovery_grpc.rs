//! GRP-25 (#461): a unary gRPC call through the sidecar is relayed unchanged and recorded with
//! its redacted messages and trailers, against a real `h2c` gRPC server.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use common::{capture_logs, counted, fixture_discovery};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::discovery::report::Direction as CoreDirection;
use mimic_core::grpc::frame::encode_frame;
use mimic_core::grpc::wire::{Field, Message, Value};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(10);
const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJDQU5BUllKV1QifQ.c2lnbmF0dXJlQ0FOQVJZ";

fn framed(fields: Vec<Field>) -> Vec<u8> {
    let payload = Message { fields }.encode().expect("encodes");
    encode_frame(false, &payload).expect("frames")
}

fn request_message() -> Vec<u8> {
    framed(vec![
        Field {
            number: 1,
            value: Value::Str("alice".into()),
        },
        Field {
            number: 2,
            value: Value::Str(JWT.into()),
        },
    ])
}

fn response_message() -> Vec<u8> {
    framed(vec![
        Field {
            number: 1,
            value: Value::Str("ghp_CANARYPREFIXabcdefghijklmnopqrstuvwxyz0".into()),
        },
        Field {
            number: 2,
            value: Value::Varint(7),
        },
    ])
}

/// A gRPC server: every call is answered with the response message and the trailers
/// `grpc-status: 0`, `x-trace-bin` and `x-request-id`.
async fn server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let _ = req.into_body().collect().await;
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", "0".parse().unwrap());
                    trailers.insert("x-trace-bin", "Q0FOQVJZVFJBQ0U".parse().unwrap());
                    trailers.insert("x-request-id", "r-1".parse().unwrap());
                    let body = Full::new(Bytes::from(response_message()))
                        .with_trailers(std::future::ready(Some(Ok::<_, Infallible>(trailers))));
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/grpc")
                            .body(body)
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .auto_date_header(false)
                    .serve_connection(TokioIo::new(s), service)
                    .await;
            });
        }
    });
    addr
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

/// One call; the response body, and its trailers.
async fn call(
    sender: &mut SendRequest<Full<Bytes>>,
    content_type: &str,
    body: Vec<u8>,
) -> (Vec<u8>, HeaderMap) {
    let req = Request::post("http://svc.ns:50051/pkg.Users/Get")
        .header("content-type", content_type)
        .header("te", "trailers")
        .header("authorization", "Bearer CANARY-BEARER")
        .header("x-note-bin", "Q0FOQVJZ")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(response.status(), 200);
    let collected = response.collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    (collected.to_bytes().to_vec(), trailers)
}

fn sum(text: &str, labels: &[&str]) -> u64 {
    text.lines()
        .filter(|l| {
            l.starts_with("mimic_discovery_bodies_total") && labels.iter().all(|x| l.contains(x))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

#[tokio::test]
async fn grp_25_a_unary_call_is_relayed_unchanged_and_recorded_redacted() {
    let logs = capture_logs();
    let app = server().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    let (body, trailers) = call(&mut sender, "application/grpc", request_message()).await;
    // The client gets what the server sent, trailers included.
    assert_eq!(body, response_message());
    assert_eq!(trailers.get("grpc-status").unwrap(), "0");
    assert_eq!(trailers.get("x-trace-bin").unwrap(), "Q0FOQVJZVFJBQ0U");
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;

    let report = discovery.report();
    assert_eq!(report.version(), 3);
    let endpoint = &report.endpoints()[0];
    assert_eq!(endpoint.direction, CoreDirection::Inbound);
    assert!(endpoint.grpc);
    let json = report.to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let samples = find_samples(&value);
    assert_eq!(samples.len(), 1);
    let sample = samples[0];
    assert_eq!(sample["request"]["body"]["value"]["1"], "alice");
    assert_eq!(sample["request"]["body"]["value"]["2"], "mimic-jwt");
    assert_eq!(sample["response"]["body"]["value"]["1"], "mimic-token");
    assert_eq!(sample["response"]["body"]["value"]["2"], 7);
    assert_eq!(
        sample["response"]["trailers"],
        serde_json::json!([
            ["grpc-status", "0"],
            ["x-trace-bin", "mimic-secret"],
            ["x-request-id", "r-1"]
        ])
    );
    for canary in [
        "CANARY-BEARER",
        "Q0FOQVJZ",
        "CANARYPREFIX",
        "c2lnbmF0dXJlQ0FOQVJZ",
        "CANARYTRACE",
    ] {
        assert!(!json.contains(canary), "a canary is in the report");
        assert!(!f.metrics.encode().contains(canary));
    }
    logs.settled().await;
    assert!(!logs.text().contains("CANARYPREFIX"));
    let text = f.metrics.encode();
    for part in ["request", "response"] {
        assert_eq!(
            sum(
                &text,
                &[&format!("part=\"{part}\""), "result=\"parsed\"", "inbound"]
            ),
            1,
            "{part}"
        );
    }
}

fn find_samples(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut out = Vec::new();
    fn walk<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Object(samples)) = map.get("samples") {
                    out.extend(samples.values());
                }
                map.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

#[tokio::test]
async fn grp_25_other_encodings_media_and_sizes_are_counted_not_kept() {
    let _logs = capture_logs();
    let app = server().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    // grpc-web: opaque. Both sides carry the media type the call sent.
    call(&mut sender, "application/grpc-web", request_message()).await;
    // An oversize request message: its frame announces 256 KiB + 1.
    let mut big = vec![0];
    big.extend(((256 * 1024 + 1) as u32).to_be_bytes());
    big.extend(vec![0u8; 256 * 1024 + 1]);
    call(&mut sender, "application/grpc", big).await;
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 2",
    )
    .await;
    let text = f.metrics.encode();
    assert_eq!(sum(&text, &["part=\"request\"", "result=\"opaque\""]), 1);
    assert_eq!(sum(&text, &["part=\"request\"", "result=\"too_large\""]), 1);
}

#[tokio::test]
async fn grp_37_a_server_that_speaks_first_with_h2_settings_is_still_h2_and_recorded() {
    let _logs = capture_logs();
    let app = server().await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    // An HTTP/2 server writes its SETTINGS as soon as it accepts (RFC 9113 §3.4: both sides
    // send a preface), and the client's preface comes a few milliseconds later.
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let (body, _) = call(&mut sender, "application/grpc", request_message()).await;
    assert_eq!(body, response_message());
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    assert_eq!(discovery.report().version(), 3);
}

#[tokio::test]
async fn grp_37_a_server_first_banner_is_tunnelled_with_no_added_wait() {
    let _logs = capture_logs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = tokio::io::AsyncWriteExt::write_all(&mut s, b"220 hello\r\n").await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    // The first connection warms the runtime; the second is timed.
    for timed in [false, true] {
        let mut tcp = TcpStream::connect(f.addr).await.unwrap();
        let started = std::time::Instant::now();
        let mut banner = [0u8; 11];
        tokio::time::timeout(
            TIMEOUT,
            tokio::io::AsyncReadExt::read_exact(&mut tcp, &mut banner),
        )
        .await
        .expect("the banner arrives")
        .unwrap();
        assert_eq!(&banner, b"220 hello\r\n");
        if timed {
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "{:?}",
                started.elapsed()
            );
        }
    }
}
