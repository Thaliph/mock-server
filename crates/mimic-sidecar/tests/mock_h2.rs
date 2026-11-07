//! MCK-12 (mocked h2c and gRPC), MCK-15 (latency, h2 part) and the head limits of MCK-18.
//! Every wait has a deadline.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::mock_rig::{AUTHORITY, Rig, rig, users_set};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http2::SendRequest;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

async fn h2c(addr: SocketAddr) -> SendRequest<Full<Bytes>> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let (send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    send
}

fn get(n: usize) -> Request<Full<Bytes>> {
    Request::builder()
        .uri(format!("http://{AUTHORITY}/users/{n}"))
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn grpc_call(path: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri(format!("http://{AUTHORITY}{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(Bytes::from_static(&[0, 0, 0, 0, 0])))
        .unwrap()
}

async fn send(
    sender: &mut SendRequest<Full<Bytes>>,
    request: Request<Full<Bytes>>,
) -> (Response<()>, Bytes, Option<hyper::HeaderMap>) {
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(request))
        .await
        .expect("answered in time")
        .unwrap();
    let (parts, body) = response.into_parts();
    let collected = tokio::time::timeout(TIMEOUT, body.collect())
        .await
        .expect("body in time")
        .unwrap();
    let trailers = collected.trailers().cloned();
    (
        Response::from_parts(parts, ()),
        collected.to_bytes(),
        trailers,
    )
}

// --- MCK-12 -----------------------------------------------------------------------------

#[tokio::test]
async fn mck_12_thirty_two_concurrent_streams_are_answered_on_one_connection() {
    let rig = rig(users_set(5), 100, 1024).await;
    let sender = h2c(rig.addr).await;
    let tasks: Vec<_> = (1..=32)
        .map(|n| {
            let mut sender = sender.clone();
            tokio::spawn(async move { (n, send(&mut sender, get(n % 12 + 1)).await) })
        })
        .collect();
    for task in tasks {
        let (n, (head, body, _)) = task.await.unwrap();
        assert_eq!(head.status(), 200, "stream {n}");
        let tier = head.headers().get("x-mimic-match").expect("x-mimic-match");
        assert!(["exact", "endpoint", "fallback"].contains(&tier.to_str().unwrap()));
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["name"], "user");
    }
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.counter("mimic_mock_connections_total{decision=\"mocked\"}"),
        1
    );
    let access = rig.logs.access_exactly(32).await;
    assert!(access.iter().all(|l| l["protocol"] == "h2"), "{access:?}");
    assert_eq!(rig.counter("mimic_mock_pending"), 0);
}

#[tokio::test]
async fn mck_12_a_miss_on_h2_is_599_with_the_miss_header() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut sender = h2c(rig.addr).await;
    let request = Request::builder()
        .uri("http://other.test:8080/users/3")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let (head, body, _) = send(&mut sender, request).await;
    assert_eq!(head.status(), 599);
    assert_eq!(head.headers()["x-mimic-miss"], "1");
    assert!(body.is_empty());
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_12_a_unary_grpc_call_gets_the_default_message_and_status_zero() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut sender = h2c(rig.addr).await;
    let (head, body, trailers) = send(&mut sender, grpc_call("/pkg.Greeter/Hello")).await;
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-type"], "application/grpc");
    assert_eq!(body.as_ref(), &[0, 0, 0, 0, 0]);
    let trailers = trailers.expect("trailers");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["protocol"], "h2");
    assert_eq!(access[0]["grpc_status"], 0);
}

#[tokio::test]
async fn mck_12_an_unrecorded_grpc_method_is_599_not_ok() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut sender = h2c(rig.addr).await;
    let (head, body, trailers) = send(&mut sender, grpc_call("/pkg.Greeter/Missing")).await;
    assert_eq!(head.status(), 599);
    assert_eq!(head.headers()["x-mimic-miss"], "1");
    assert!(body.is_empty());
    assert!(trailers.is_none_or(|t| !t.contains_key("grpc-status")));
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_12_mocked_h2_connections_advertise_256_concurrent_streams() {
    let rig = rig(users_set(5), 100, 1024).await;
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (send, conn) = h2::client::handshake(tcp).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let send = tokio::time::timeout(TIMEOUT, send.ready())
        .await
        .unwrap()
        .unwrap();
    // Until the server's SETTINGS arrive the limit is unbounded.
    for _ in 0..100 {
        if send.current_max_send_streams() != usize::MAX {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(send.current_max_send_streams(), 256);
}

// --- MCK-15 -----------------------------------------------------------------------------

#[tokio::test]
async fn mck_15_thirty_two_h2_streams_wait_the_drawn_latency_concurrently() {
    // 200 ms is in bucket 31 (182 to 215 ms).
    let rig = rig(users_set(200), 100, 1024).await;
    let sender = h2c(rig.addr).await;
    let begun = Instant::now();
    let tasks: Vec<_> = (1..=32)
        .map(|n| {
            let mut sender = sender.clone();
            tokio::spawn(async move {
                let (head, _, _) = send(&mut sender, get(n % 12 + 1)).await;
                (begun.elapsed(), head.status())
            })
        })
        .collect();
    let mut times = Vec::new();
    for task in tasks {
        let (elapsed, status) = task.await.unwrap();
        assert_eq!(status, 200);
        assert!(elapsed >= Duration::from_millis(182), "{elapsed:?}");
        times.push(elapsed);
    }
    let first = times.iter().min().unwrap();
    let last = times.iter().max().unwrap();
    assert!(
        *last - *first <= Duration::from_millis(100),
        "{first:?} {last:?}"
    );
    for line in rig.logs.access_exactly(32).await {
        let head = line["head_ms"].as_u64().unwrap();
        assert!((182..=265).contains(&head), "head_ms {head}");
    }
}

#[tokio::test]
async fn mck_15_h2_misses_are_answered_without_delay() {
    let rig = rig(users_set(200), 100, 1024).await;
    let mut sender = h2c(rig.addr).await;
    let request = Request::builder()
        .uri("http://other.test:8080/x")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let begun = Instant::now();
    let (head, _, _) = send(&mut sender, request).await;
    assert_eq!(head.status(), 599);
    assert!(
        begun.elapsed() < Duration::from_millis(50),
        "{:?}",
        begun.elapsed()
    );
}

// --- MCK-18: the head limits of the mocked server ---------------------------------------

async fn closed_unframed(rig: &Rig, raw: Vec<u8>) {
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    let _ = s.write_all(&raw).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, s.read_to_end(&mut out))
        .await
        .expect("closed in time");
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["result"], "unframed");
}

#[tokio::test]
async fn mck_18_a_head_over_64_kib_closes_the_connection() {
    let rig = rig(users_set(5), 100, 1024).await;
    let raw = format!(
        "GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\nX-Pad: {}\r\n\r\n",
        "a".repeat(100 * 1024)
    );
    closed_unframed(&rig, raw.into_bytes()).await;
}

#[tokio::test]
async fn mck_18_a_head_of_300_lines_closes_the_connection() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut raw = format!("GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\n");
    for i in 0..300 {
        raw.push_str(&format!("X-{i}: v\r\n"));
    }
    raw.push_str("\r\n");
    closed_unframed(&rig, raw.into_bytes()).await;
}

#[tokio::test]
async fn mck_18_a_head_of_200_short_lines_is_served() {
    let rig = rig(users_set(5), 100, 1024).await;
    let mut raw = format!("GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\n");
    for i in 0..200 {
        raw.push_str(&format!("X-{i}: v\r\n"));
    }
    raw.push_str("\r\n");
    let mut s = TcpStream::connect(rig.addr).await.unwrap();
    s.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
}
