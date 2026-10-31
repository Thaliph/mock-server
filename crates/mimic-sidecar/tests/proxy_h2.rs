//! PRX-04, PRX-07 (HTTP/2 part): the HTTP/2 prior-knowledge relay and gRPC, in process.

mod common;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{capture_logs, closed_by_peer, closed_port, counted, fixture};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::listener::Direction;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(5);
const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

type Body = BoxBody<Bytes, io::Error>;

fn full(data: &'static str) -> Body {
    Full::new(Bytes::from_static(data.as_bytes()))
        .map_err(|never| match never {})
        .boxed()
}

/// An `h2c` destination running `handler` for every request; counts its connections.
async fn dest<F, Fut>(handler: F) -> (SocketAddr, Arc<AtomicUsize>)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<Response<Body>, io::Error>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    tokio::spawn(async move {
        loop {
            let (s, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .auto_date_header(false)
                    .serve_connection(TokioIo::new(s), service_fn(handler))
                    .await;
            });
        }
    });
    (addr, connections)
}

async fn client(proxy: SocketAddr) -> SendRequest<Body> {
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    sender
}

fn sorted(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut all: Vec<_> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    all.sort();
    all
}

#[tokio::test]
async fn prx_04_concurrent_streams_share_one_destination_connection() {
    for direction in [Direction::Inbound, Direction::Outbound] {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (addr, connections) = dest(move |req: Request<Incoming>| {
            let (barrier, tx) = (barrier.clone(), tx.clone());
            async move {
                let (parts, body) = req.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                tx.send((
                    parts.method.to_string(),
                    parts.uri.to_string(),
                    sorted(&parts.headers),
                    body,
                ))
                .unwrap();
                // All three streams must be in flight at the same time to get an answer.
                barrier.wait().await;
                let response = Response::builder()
                    .header("x-resp", "v")
                    .header("x-resp", "w")
                    .body(Full::new(Bytes::from(parts.uri.path().to_string())))
                    .unwrap();
                Ok(response.map(|b| b.map_err(|never| match never {}).boxed()))
            }
        })
        .await;
        let f = fixture(direction, move |_| Some(addr)).await;
        let sender = client(f.addr).await;

        let mut calls = Vec::new();
        for i in 0..3 {
            let mut sender = sender.clone();
            calls.push(tokio::spawn(async move {
                let payload = format!("body-{i}");
                let req = Request::post(format!("http://svc.example/p{i}?q={i}"))
                    .header("x-custom", "one")
                    .header("x-custom", "two")
                    .header("content-type", "application/octet-stream")
                    .header("content-length", payload.len())
                    .body(
                        Full::new(Bytes::from(payload))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap();
                let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
                    .await
                    .expect("answered")
                    .unwrap();
                let (parts, body) = response.into_parts();
                (parts, body.collect().await.unwrap().to_bytes())
            }));
        }
        for (i, call) in calls.into_iter().enumerate() {
            let (parts, body) = call.await.unwrap();
            assert_eq!(parts.status, 200);
            assert_eq!(body, format!("/p{i}"));
            let names: Vec<_> = sorted(&parts.headers).into_iter().map(|(k, _)| k).collect();
            assert!(
                !names
                    .iter()
                    .any(|n| n == "date" || n == "via" || n == "server"),
                "{names:?}"
            );
            assert_eq!(parts.headers.get_all("x-resp").iter().count(), 2);
        }
        for _ in 0..3 {
            let (method, uri, headers, body) = seen.recv().await.unwrap();
            let i = body
                .strip_prefix(b"body-")
                .map(|n| n[0] - b'0')
                .expect("body relayed");
            assert_eq!(method, "POST");
            assert_eq!(uri, format!("http://svc.example/p{i}?q={i}"));
            let len = format!("body-{i}").len().to_string();
            let expected: Vec<(String, String)> = [
                ("content-length", len.as_str()),
                ("content-type", "application/octet-stream"),
                ("x-custom", "one"),
                ("x-custom", "two"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .to_vec();
            assert_eq!(headers, expected, "no header added or removed");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn prx_04_unary_grpc_keeps_message_and_trailers() {
    let logs = capture_logs();
    let (tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let (addr, _) = dest(move |req: Request<Incoming>| {
        let tx = tx.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.unwrap().to_bytes();
            tx.send((parts.uri.path().to_string(), sorted(&parts.headers), body))
                .unwrap();
            let mut trailers = HeaderMap::new();
            trailers.insert("grpc-status", "3".parse().unwrap());
            trailers.insert("grpc-message", "bad%20input".parse().unwrap());
            let message = Bytes::from_static(b"\x00\x00\x00\x00\x03abc");
            let body = Full::new(message)
                .with_trailers(async move { Some(Ok::<_, std::convert::Infallible>(trailers)) })
                .map_err(|never| match never {})
                .boxed();
            Ok(Response::builder()
                .header("content-type", "application/grpc")
                .body(body)
                .unwrap())
        }
    })
    .await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut sender = client(f.addr).await;
    let req = Request::post("http://greeter.example:50051/helloworld.Greeter/SayHello")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("grpc-timeout", "5S")
        .body(
            Full::new(Bytes::from_static(b"\x00\x00\x00\x00\x02hi"))
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap();
    let response = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .expect("answered")
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/grpc");
    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().expect("trailers relayed");
    assert_eq!(trailers["grpc-status"], "3");
    assert_eq!(trailers["grpc-message"], "bad%20input");
    assert_eq!(collected.to_bytes(), &b"\x00\x00\x00\x00\x03abc"[..]);

    let (path, headers, body) = seen.recv().await.unwrap();
    assert_eq!(path, "/helloworld.Greeter/SayHello");
    assert_eq!(body, &b"\x00\x00\x00\x00\x02hi"[..]);
    let names: Vec<_> = headers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !names
            .iter()
            .any(|n| ["date", "via", "server", "forwarded"].contains(n)),
        "{names:?}"
    );
    assert!(names.contains(&"grpc-timeout") && names.contains(&"te"));

    let lines = logs.access_exactly(1).await;
    let line = &lines[0];
    assert_eq!(line["protocol"], "h2");
    assert_eq!(line["authority"], "greeter.example:50051");
    assert_eq!(line["method"], "POST");
    assert_eq!(line["path"], "/helloworld.Greeter/SayHello");
    assert_eq!(line["status"], 200);
    assert_eq!(line["grpc_status"], 3);
    assert_eq!(line["result"], "ok");
    assert_eq!(line["bytes_in"], 7);
    assert_eq!(line["bytes_out"], 8);
    counted(
        &f.metrics,
        "mimic_http_requests_total{direction=\"outbound\",protocol=\"h2\",authority=\"greeter.example:50051\",status_class=\"2xx\"} 1",
    )
    .await;
    counted(
        &f.metrics,
        "mimic_proxy_connections_total{direction=\"outbound\",protocol=\"h2\"",
    )
    .await;
}

#[tokio::test]
async fn prx_07_h2_destination_reset_resets_only_that_stream() {
    let logs = capture_logs();
    let ok_arrived = Arc::new(tokio::sync::Notify::new());
    let reset_done = Arc::new(tokio::sync::Notify::new());
    let (addr, _) = dest(move |req: Request<Incoming>| {
        let (ok_arrived, reset_done) = (ok_arrived.clone(), reset_done.clone());
        async move {
            if req.uri().path() == "/reset" {
                ok_arrived.notified().await;
                reset_done.notify_one();
                return Err(io::Error::other("destination gives up"));
            }
            ok_arrived.notify_one();
            reset_done.notified().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(Response::new(full("fine")))
        }
    })
    .await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let sender = client(f.addr).await;
    let call = |path: &'static str| {
        let mut sender = sender.clone();
        async move {
            let req = Request::get(format!("http://a.example{path}"))
                .body(full(""))
                .unwrap();
            let response = sender.send_request(req).await?;
            let status = response.status();
            let body = response.into_body().collect().await?.to_bytes();
            Ok::<_, hyper::Error>((status, body))
        }
    };
    let (ok, reset) =
        tokio::time::timeout(TIMEOUT, async { tokio::join!(call("/ok"), call("/reset")) })
            .await
            .expect("both streams end");
    let (status, body) = ok.expect("the other stream completes");
    assert_eq!((status.as_u16(), body), (200, Bytes::from_static(b"fine")));
    assert!(reset.is_err(), "the failed stream is reset: {reset:?}");

    let lines = logs.access_exactly(2).await;
    let failed = lines.iter().find(|l| l["path"] == "/reset").unwrap();
    assert_eq!(failed["result"], "upstream_reset");
    assert!(failed.get("status").is_none(), "{failed}");
    let passed = lines.iter().find(|l| l["path"] == "/ok").unwrap();
    assert_eq!(passed["result"], "ok");
    counted(
        &f.metrics,
        "mimic_http_requests_total{direction=\"outbound\",protocol=\"h2\",authority=\"a.example\",status_class=\"none\"} 1",
    )
    .await;
}

#[tokio::test]
async fn prx_07_h2_refused_destination_closes_without_a_byte() {
    let dst = closed_port().await;
    let f = fixture(Direction::Outbound, move |_| Some(dst)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(PREFACE).await.unwrap();
    closed_by_peer(c).await;
}

#[tokio::test]
async fn prx_23_h2_authority_wins_over_host() {
    let logs = capture_logs();
    let (addr, _) = dest(|_req: Request<Incoming>| async { Ok(Response::new(full("ok"))) }).await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut sender = client(f.addr).await;
    let req = Request::get("http://a.example/")
        .header("host", "b.example")
        .body(full(""))
        .unwrap();
    let res = tokio::time::timeout(TIMEOUT, sender.send_request(req))
        .await
        .unwrap()
        .unwrap();
    res.into_body().collect().await.unwrap();
    counted(
        &f.metrics,
        "mimic_http_requests_total{direction=\"outbound\",protocol=\"h2\",authority=\"a.example\",status_class=\"2xx\"} 1",
    )
    .await;
    let lines = logs.access_exactly(1).await;
    assert_eq!(lines[0]["authority"], "a.example");
    assert!(!f.metrics.encode().contains("b.example"));
}
