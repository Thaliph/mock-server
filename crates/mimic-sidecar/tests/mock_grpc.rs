//! GRP-33 (#463): a mocked unary gRPC call is answered over h2c with the recorded response
//! message, status and trailers. Every wait has a deadline.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

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
use mimic_core::grpc::wire::Message;
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);
const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJDQU5BUllKV1QifQ.c2lnbmF0dXJlQ0FOQVJZ";

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

/// One recorded call of `path`.
fn record(
    report: &mut Report,
    path: &str,
    request: &str,
    response: Option<&str>,
    headers: &[(&str, &str)],
    trailers: &[(&str, &str)],
) {
    let (request, response) = (framed(request), response.map(framed));
    let req = [("content-type", "application/grpc")];
    let mut resp = vec![("content-type", "application/grpc")];
    resp.extend_from_slice(headers);
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
            path,
            status: 200,
            head_ms: 1,
            host: None,
            grpc: true,
            observation: Observation {
                query: "",
                req_headers: &req,
                req_body: Some(&request),
                resp_headers: &resp,
                resp_body: response.as_deref(),
                resp_trailers: trailers,
                resp_marks: &[],
            },
        },
    );
}

fn recorded() -> EgressSet {
    let mut report = Report::new(None);
    let trailers = [("grpc-status", "0"), ("x-neutral", "v")];
    record(
        &mut report,
        "/pkg.Greeter/Hello",
        r#"{"1":"alice"}"#,
        Some(&format!(r#"{{"1":"hello alice","2":"{JWT}","3":7}}"#)),
        &[],
        &trailers,
    );
    record(
        &mut report,
        "/pkg.Greeter/Hello",
        r#"{"1":"bob"}"#,
        Some(r#"{"1":"hello bob"}"#),
        &[],
        &trailers,
    );
    // A failed call: trailers only, the status in the response headers, on a method of its own:
    // a field that three calls of one method give mostly different values is volatile and never
    // compared (spec §6), so the matching above is tried with two calls.
    record(
        &mut report,
        "/pkg.Greeter/Fail",
        r#"{"1":"nobody"}"#,
        None,
        &[("grpc-status", "5"), ("grpc-message", "not found")],
        &[],
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

async fn call(
    sender: &mut SendRequest<Full<Bytes>>,
    path: &str,
    body: Vec<u8>,
) -> (Response<()>, Bytes, Option<HeaderMap>) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{AUTHORITY}{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
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

#[tokio::test]
async fn grp_33_a_unary_call_gets_the_recorded_message_and_trailers() {
    let rig = rig(recorded(), 100, 1024).await;
    let mut sender = h2c(&rig).await;
    let (head, body, trailers) = call(
        &mut sender,
        "/pkg.Greeter/Hello",
        framed(r#"{"1":"alice"}"#),
    )
    .await;
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-type"], "application/grpc");
    assert_eq!(head.headers()["x-mimic-match"], "exact");
    // The message as recorded: the redacted JWT is the placeholder, sent as a string.
    assert_eq!(
        unframed(&body),
        r#"{"1":"hello alice","2":"mimic-jwt","3":7}"#
    );
    let trailers = trailers.expect("trailers");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["x-neutral"], "v");
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["grpc_status"], 0);
    assert_eq!(access[0]["mock"], "exact");
}

#[tokio::test]
async fn grp_33_another_request_message_gets_its_own_sample() {
    let rig = rig(recorded(), 100, 1024).await;
    let mut sender = h2c(&rig).await;
    let (_, body, _) = call(&mut sender, "/pkg.Greeter/Hello", framed(r#"{"1":"bob"}"#)).await;
    assert_eq!(unframed(&body), r#"{"1":"hello bob"}"#);
}

#[tokio::test]
async fn grp_33_a_recorded_error_status_is_replayed_with_no_frame() {
    let rig = rig(recorded(), 100, 1024).await;
    let mut sender = h2c(&rig).await;
    let (head, body, trailers) = call(
        &mut sender,
        "/pkg.Greeter/Fail",
        framed(r#"{"1":"nobody"}"#),
    )
    .await;
    assert_eq!(head.status(), 200);
    assert!(head.headers().get("grpc-status").is_none());
    assert!(body.is_empty());
    let trailers = trailers.expect("trailers");
    assert_eq!(trailers["grpc-status"], "5");
    assert_eq!(trailers["grpc-message"], "not found");
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["grpc_status"], 5);
}

#[tokio::test]
async fn grp_33_an_unrecorded_method_is_599() {
    let rig = rig(recorded(), 100, 1024).await;
    let mut sender = h2c(&rig).await;
    let (head, body, _) = call(&mut sender, "/pkg.Greeter/Other", framed(r#"{"1":"a"}"#)).await;
    assert_eq!(head.status(), 599);
    assert_eq!(head.headers()["x-mimic-miss"], "1");
    assert!(body.is_empty());
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}
