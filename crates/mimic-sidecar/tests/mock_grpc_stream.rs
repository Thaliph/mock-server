//! GRP-44 and GRP-45 (#464): a recorded server-streaming call is replayed over h2c with its
//! pace, and a client that cancels stops the replay. Every wait has a deadline.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::mock_rig::{AUTHORITY, Rig, limits, rig};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http2::SendRequest;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::discovery::egress::EgressSet;
use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_core::grpc::frame::{Parser, encode_frame};
use mimic_core::grpc::record::Mark;
use mimic_core::grpc::wire::Message;
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);
const PATH: &str = "/pkg.Greeter/Watch";

fn framed(json: &str) -> Vec<u8> {
    let value: serde_json::Value = serde_json::from_str(json).expect("json");
    let payload = Message::from_json(&value)
        .expect("a message")
        .encode()
        .expect("encodes");
    encode_frame(false, &payload).expect("a frame")
}

fn unframed(body: &[u8]) -> String {
    let pushed = Parser::new().push(body);
    assert_eq!(pushed.frames.len(), 1);
    Message::decode(&pushed.frames[0].payload)
        .expect("a message")
        .to_json_string()
}

/// A recorded call of five messages `{"1": n}` that arrived 100 ms apart, then the trailers.
fn recorded() -> EgressSet {
    let request = framed(r#"{"1":"watch"}"#);
    let mut body = Vec::new();
    let mut marks = Vec::new();
    for n in 1..=5u64 {
        body.extend(framed(&format!(r#"{{"1":{n}}}"#)));
        marks.push(Mark {
            end: body.len(),
            ms: 100 * (n - 1),
        });
    }
    let req = [("content-type", "application/grpc")];
    let resp = [("content-type", "application/grpc")];
    let trailers = [
        ("grpc-status", "0"),
        ("x-neutral", "v"),
        ("x-trace-bin", "AAEC"),
    ];
    let mut report = Report::new(None);
    report.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &Via {
            tls: false,
            sni: None,
            dst: None,
        },
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 8080,
            method: "POST",
            path: PATH,
            status: 200,
            head_ms: 1,
            host: None,
            grpc: true,
            observation: Observation {
                query: "",
                req_headers: &req,
                req_body: Some(&request),
                resp_headers: &resp,
                resp_body: Some(&body),
                resp_trailers: &trailers,
                resp_marks: &marks,
            },
        },
    );
    EgressSet::project(&report)
}

async fn h2c(rig: &Rig) -> SendRequest<Full<Bytes>> {
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    send
}

async fn start(sender: &mut SendRequest<Full<Bytes>>) -> Response<hyper::body::Incoming> {
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{AUTHORITY}{PATH}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(Bytes::from(framed(r#"{"1":"watch"}"#))))
        .unwrap();
    tokio::time::timeout(TIMEOUT, sender.send_request(request))
        .await
        .expect("answered in time")
        .unwrap()
}

/// What arrived: the instant of each data frame, the frames, the trailers.
async fn read_all(
    response: Response<hyper::body::Incoming>,
) -> (Vec<Instant>, Vec<Bytes>, Option<HeaderMap>) {
    let mut body = response.into_body();
    let (mut at, mut data, mut trailers) = (Vec::new(), Vec::new(), None);
    while let Some(frame) = tokio::time::timeout(TIMEOUT, body.frame())
        .await
        .expect("a frame in time")
    {
        let frame = frame.unwrap();
        match frame.into_data() {
            Ok(bytes) => {
                at.push(Instant::now());
                data.push(bytes);
            }
            Err(frame) => trailers = frame.into_trailers().ok(),
        }
    }
    (at, data, trailers)
}

fn gaps(at: &[Instant]) -> Vec<u128> {
    at.windows(2).map(|w| (w[1] - w[0]).as_millis()).collect()
}

async fn replayed(factor: u32) -> (Rig, Vec<Instant>, Vec<Bytes>, Option<HeaderMap>) {
    let rig = rig(recorded(), factor, 1024).await;
    let mut sender = h2c(&rig).await;
    let response = start(&mut sender).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/grpc");
    assert_eq!(response.headers()["x-mimic-match"], "exact");
    assert!(response.headers().get("content-length").is_none());
    let (at, data, trailers) = read_all(response).await;
    (rig, at, data, trailers)
}

#[tokio::test]
async fn grp_44_the_messages_come_in_order_at_the_recorded_pace_then_the_trailers() {
    let (rig, at, data, trailers) = replayed(100).await;
    let messages: Vec<String> = data.iter().map(|d| unframed(d)).collect();
    assert_eq!(
        messages,
        [
            r#"{"1":1}"#,
            r#"{"1":2}"#,
            r#"{"1":3}"#,
            r#"{"1":4}"#,
            r#"{"1":5}"#
        ]
    );
    for gap in gaps(&at) {
        assert!((88..=128).contains(&gap), "{gap}");
    }
    let trailers = trailers.expect("trailers");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["x-neutral"], "v");
    assert!(trailers.get("x-trace-bin").is_none());
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["mock"], "exact");
    assert_eq!(access[0]["grpc_status"], 0);
    assert_eq!(rig.counter("mimic_mock_pending "), 0);
}

#[tokio::test]
async fn grp_44_the_latency_factor_scales_the_pace() {
    let (_, at, data, _) = replayed(200).await;
    assert_eq!(data.len(), 5);
    for gap in gaps(&at) {
        assert!((196..=236).contains(&gap), "{gap}");
    }
    let (_, at, data, trailers) = replayed(0).await;
    assert_eq!(data.len(), 5);
    assert!(
        at[4] - at[0] < Duration::from_millis(50),
        "{:?}",
        at[4] - at[0]
    );
    assert!(trailers.is_some());
}

#[tokio::test]
async fn grp_44_the_stream_holds_its_pending_slot_until_it_ends() {
    let rig = rig(recorded(), 100, 1024).await;
    let mut sender = h2c(&rig).await;
    let response = start(&mut sender).await;
    assert_eq!(rig.counter("mimic_mock_pending "), 1);
    let (_, data, _) = read_all(response).await;
    assert_eq!(data.len(), 5);
    let deadline = Instant::now() + Duration::from_secs(2);
    while rig.counter("mimic_mock_pending ") != 0 {
        assert!(Instant::now() < deadline, "the slot is not given back");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn pending_is_zero_within(rig: &Rig, within: Duration) {
    let deadline = Instant::now() + within;
    while rig.counter("mimic_mock_pending ") != 0 {
        assert!(Instant::now() < deadline, "the stream still holds its slot");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn grp_45_a_reset_stream_stops_the_replay_and_frees_its_slot() {
    // 540 ms between messages: the cancel comes well before the third.
    let rig = rig(recorded(), 500, 1024).await;
    let mut sender = h2c(&rig).await;
    let response = start(&mut sender).await;
    let mut body = response.into_body();
    for _ in 0..2 {
        let frame = tokio::time::timeout(TIMEOUT, body.frame())
            .await
            .expect("a frame")
            .expect("a frame")
            .unwrap();
        assert!(frame.is_data());
    }
    assert_eq!(rig.counter("mimic_mock_pending "), 1);
    // Dropping the body resets the stream (`RST_STREAM`).
    drop(body);
    pending_is_zero_within(&rig, Duration::from_millis(450)).await;
    // The connection still serves a call.
    let again = start(&mut sender).await;
    assert_eq!(again.status(), 200);
    drop(again);
    pending_is_zero_within(&rig, Duration::from_millis(450)).await;
}

#[tokio::test]
async fn grp_45_many_reset_streams_leave_nothing_behind() {
    let rig = rig(recorded(), 10_000, 1024).await;
    let mut sender = h2c(&rig).await;
    for _ in 0..100 {
        let response = start(&mut sender).await;
        let mut body = response.into_body();
        let frame = tokio::time::timeout(TIMEOUT, body.frame())
            .await
            .expect("a frame")
            .expect("a frame")
            .unwrap();
        assert!(frame.is_data());
    }
    pending_is_zero_within(&rig, Duration::from_millis(900)).await;
}

#[tokio::test]
async fn grp_45_a_closed_connection_stops_the_replay() {
    let rig = rig(recorded(), 10_000, 1024).await;
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    let task = tokio::spawn(conn);
    let response = start(&mut sender).await;
    let mut body = response.into_body();
    let frame = tokio::time::timeout(TIMEOUT, body.frame())
        .await
        .expect("a frame")
        .expect("a frame")
        .unwrap();
    assert!(frame.is_data());
    assert_eq!(rig.counter("mimic_mock_pending "), 1);
    task.abort();
    drop((body, sender));
    pending_is_zero_within(&rig, Duration::from_millis(900)).await;
}
