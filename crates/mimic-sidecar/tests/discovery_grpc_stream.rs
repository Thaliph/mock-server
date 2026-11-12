//! GRP-42 (#464): a server-streaming gRPC call through the sidecar is relayed unchanged and
//! recorded as a sequence of redacted messages with bucketed delays, against a real `h2c`
//! gRPC server. Client and bidirectional streams stay recorded by their first message.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use common::{capture_logs, counted, fixture_discovery};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::grpc::frame::encode_frame;
use mimic_core::grpc::wire::{Field, Message, Value};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(10);
const GAP: Duration = Duration::from_millis(100);

fn framed(fields: Vec<Field>) -> Vec<u8> {
    let payload = Message { fields }.encode().expect("encodes");
    encode_frame(false, &payload).expect("frames")
}

fn message(n: u64) -> Vec<u8> {
    framed(vec![
        Field {
            number: 1,
            value: Value::Varint(n),
        },
        Field {
            number: 2,
            value: Value::Str(format!("ghp_CANARYPREFIXabcdefghijklmnopqrstuvwxyz{n}")),
        },
    ])
}

/// A response body fed by a task: frames as the server decides to send them.
struct Paced(mpsc::Receiver<Frame<Bytes>>);

impl Body for Paced {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|f| f.map(Ok))
    }
}

/// A gRPC server: every call is answered with `count` messages `GAP` apart, then the trailers
/// `grpc-status: 0`, `x-trace-bin` and `x-request-id`; the messages are the same whatever the
/// request holds, and `size` bytes of padding go into each when it is not 0.
async fn server(count: u64, size: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let _ = req.into_body().collect().await;
                    let (tx, rx) = mpsc::channel(8);
                    tokio::spawn(async move {
                        for n in 1..=count {
                            let mut data = message(n);
                            if size > 0 {
                                data = framed(vec![Field {
                                    number: 1,
                                    value: Value::Bytes(vec![0xff; size]),
                                }]);
                            }
                            if tx.send(Frame::data(Bytes::from(data))).await.is_err() {
                                return;
                            }
                            if n < count {
                                tokio::time::sleep(GAP).await;
                            }
                        }
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        trailers.insert("x-trace-bin", "Q0FOQVJZVFJBQ0U".parse().unwrap());
                        trailers.insert("x-request-id", "r-1".parse().unwrap());
                        let _ = tx.send(Frame::trailers(trailers)).await;
                    });
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/grpc")
                            .body(Paced(rx))
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

/// One call; when each chunk of the response arrived, the body and the trailers.
async fn call(
    sender: &mut SendRequest<Full<Bytes>>,
    body: Vec<u8>,
) -> (Vec<(Duration, usize)>, Vec<u8>, HeaderMap) {
    let req = Request::post("http://svc.ns:50051/pkg.Users/Watch")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let started = Instant::now();
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let (mut chunks, mut all, mut trailers) = (Vec::new(), Vec::new(), HeaderMap::new());
    while let Some(frame) = tokio::time::timeout(TIMEOUT, body.frame())
        .await
        .expect("a frame")
    {
        let frame = frame.unwrap();
        match frame.into_data() {
            Ok(data) => {
                chunks.push((started.elapsed(), data.len()));
                all.extend_from_slice(&data);
            }
            Err(frame) => {
                if let Ok(t) = frame.into_trailers() {
                    trailers = t;
                }
            }
        }
    }
    (chunks, all, trailers)
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

fn sum(text: &str, labels: &[&str]) -> u64 {
    text.lines()
        .filter(|l| {
            l.starts_with("mimic_discovery_bodies_total") && labels.iter().all(|x| l.contains(x))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .map(|v| v as u64)
        .sum()
}

async fn recorded(
    app: SocketAddr,
    requests: Vec<Vec<u8>>,
) -> (Vec<serde_json::Value>, Discovery, String) {
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    let total = requests.len();
    for request in requests {
        call(&mut sender, request).await;
    }
    counted(
        &f.metrics,
        &format!(
            "mimic_discovery_exchanges_total{{direction=\"inbound\",result=\"recorded\"}} {total}"
        ),
    )
    .await;
    let json = discovery.report().to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let samples = find_samples(&value).into_iter().cloned().collect();
    (samples, discovery, f.metrics.encode())
}

#[tokio::test]
async fn grp_42_a_server_stream_is_relayed_unchanged_and_recorded_as_steps() {
    let logs = capture_logs();
    let app = server(5, 0).await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    let (chunks, body, trailers) = call(&mut sender, message(0)).await;
    // What the server sent, in order, with its trailers, paced as it was.
    let want: Vec<u8> = (1..=5).flat_map(message).collect();
    assert_eq!(body, want);
    assert_eq!(trailers.get("grpc-status").unwrap(), "0");
    assert_eq!(trailers.get("x-trace-bin").unwrap(), "Q0FOQVJZVFJBQ0U");
    assert_eq!(chunks.len(), 5, "{chunks:?}");
    for pair in chunks.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(gap >= GAP - Duration::from_millis(20), "{gap:?}");
        assert!(gap <= GAP + Duration::from_millis(100), "{gap:?}");
    }
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;

    let report = discovery.report();
    assert_eq!(report.version(), 3);
    let json = report.to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let samples = find_samples(&value);
    assert_eq!(samples.len(), 1);
    let response = &samples[0]["response"];
    assert_eq!(response["body"]["value"]["1"], 1);
    assert_eq!(response["body"]["value"]["2"], "mimic-token");
    let stream = response["stream"].as_array().expect("a stream");
    assert_eq!(stream.len(), 4);
    for (i, step) in stream.iter().enumerate() {
        assert_eq!(step["message"]["1"], i as u64 + 2);
        assert_eq!(step["message"]["2"], "mimic-token");
        // The bucket bound above 100 ms, never the exact time and never less than the gap.
        let delay = step["delay_ms"].as_u64().unwrap();
        assert!((100..=140).contains(&delay), "{delay}");
        assert_eq!(delay, mimic_core::grpc::record::bucketed(delay));
    }
    assert_eq!(
        response["trailers"][0],
        serde_json::json!(["grpc-status", "0"])
    );
    assert_eq!(
        response["trailers"][1],
        serde_json::json!(["x-trace-bin", "mimic-secret"])
    );
    assert!(!json.contains("CANARYPREFIX") && !json.contains("Q0FOQVJZ"));
    logs.settled().await;
    assert!(!logs.text().contains("CANARYPREFIX"));
    assert_eq!(
        sum(
            &f.metrics.encode(),
            &["part=\"response\"", "result=\"parsed\""]
        ),
        1
    );
}

#[tokio::test]
async fn grp_42_client_and_bidirectional_streams_are_recorded_by_their_first_message() {
    let _logs = capture_logs();
    let app = server(2, 0).await;
    // Three request messages (client streaming), two (bidirectional): one response each.
    let three: Vec<u8> = (1..=3).flat_map(message).collect();
    let two: Vec<u8> = (1..=2).flat_map(message).collect();
    let (samples, _, _) = recorded(app, vec![three, two]).await;
    assert!(!samples.is_empty());
    for sample in &samples {
        assert_eq!(sample["response"]["body"]["value"]["1"], 1);
        assert!(sample["response"].get("stream").is_none());
    }
}

#[tokio::test]
async fn grp_42_a_stream_over_256_kib_is_relayed_whole_and_recorded_without_messages() {
    let _logs = capture_logs();
    // 5 messages of 60 000 bytes: 300 000 bytes.
    let app = server(5, 60_000).await;
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let f = fixture_discovery(Direction::Inbound, &discovery, move |_| Some(app)).await;
    let mut sender = client(f.addr).await;
    let (_, body, trailers) = call(&mut sender, message(0)).await;
    assert_eq!(body.len(), 5 * 60_005);
    assert_eq!(trailers.get("grpc-status").unwrap(), "0");
    counted(
        &f.metrics,
        "mimic_discovery_exchanges_total{direction=\"inbound\",result=\"recorded\"} 1",
    )
    .await;
    let json = discovery.report().to_json().unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let samples = find_samples(&value);
    assert!(samples[0]["response"].get("stream").is_none());
    assert!(samples[0]["response"]["body"].is_null());
    assert_eq!(
        sum(
            &f.metrics.encode(),
            &["part=\"response\"", "result=\"too_large\""]
        ),
        1
    );
}
