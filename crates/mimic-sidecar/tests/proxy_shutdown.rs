//! PRX-13: graceful shutdown of the proxy, in process (`Shared::shutdown` is what the
//! sidecar's `SIGTERM` handler calls).

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{closed_by_peer, fixture};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Answers every request with `first`, waits for the `gate`, then sends `rest` and ends the
/// chunked body.
async fn streaming_dest(gate: std::sync::Arc<tokio::sync::Notify>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let head = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n";
                s.write_all(head).await.unwrap();
                gate.notified().await;
                s.write_all(b"4\r\nrest\r\n0\r\n\r\n").await.unwrap();
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink).await;
            });
        }
    });
    addr
}

async fn refused(addr: SocketAddr) {
    for _ in 0..100 {
        if TcpStream::connect(addr).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{addr} still accepts connections");
}

#[tokio::test]
async fn prx_13_streamed_response_completes_and_new_connections_are_refused() {
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let dest = streaming_dest(gate.clone()).await;
    let f = fixture(Direction::Inbound, move |_| Some(dest)).await;

    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /stream HTTP/1.1\r\nhost: svc\r\n\r\n")
        .await
        .unwrap();
    let mut seen = Vec::new();
    while !seen.ends_with(b"first\r\n") {
        let mut buf = [0u8; 512];
        let n = tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("first chunk")
            .unwrap();
        assert!(n > 0, "closed early: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..n]);
    }

    f.shared.shutdown();
    refused(f.addr).await;
    tokio::time::timeout(Duration::from_millis(200), f.shared.drained())
        .await
        .expect_err("a response is still streaming");

    gate.notify_one();
    let mut rest = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut rest))
        .await
        .expect("response completes and the connection closes")
        .unwrap();
    let text = String::from_utf8_lossy(&rest);
    assert!(
        text.contains("rest") && text.ends_with("0\r\n\r\n"),
        "{text}"
    );
    tokio::time::timeout(TIMEOUT, f.shared.drained())
        .await
        .expect("no connection left");
}

#[tokio::test]
async fn prx_13_idle_http1_connection_is_closed() {
    let dest = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
                        if s.write_all(ok).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    };
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nhost: svc\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 256];
    let mut got = Vec::new();
    while !got.ends_with(b"ok") {
        let n = tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("answered")
            .unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }

    f.shared.shutdown();
    closed_by_peer(c).await;
    tokio::time::timeout(TIMEOUT, f.shared.drained())
        .await
        .expect("no connection left");
}

#[tokio::test]
async fn prx_13_http2_client_receives_goaway() {
    let dest = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = service_fn(|_req: Request<hyper::body::Incoming>| async {
                        Ok::<_, std::io::Error>(Response::new(Empty::<Bytes>::new()))
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(s), svc)
                        .await;
                });
            }
        });
        addr
    };
    let f = fixture(Direction::Inbound, move |_| Some(dest)).await;
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(tcp))
        .await
        .unwrap();
    let conn = tokio::spawn(conn);
    let req = || {
        Request::get("http://svc.example/")
            .body(Empty::new())
            .unwrap()
    };
    let response = sender.send_request(req()).await.unwrap();
    response.into_body().collect().await.unwrap();

    f.shared.shutdown();
    let ended = tokio::time::timeout(TIMEOUT, conn)
        .await
        .expect("the connection ends after GOAWAY")
        .unwrap();
    // A GOAWAY with no error is a clean end for the client.
    assert!(ended.is_ok(), "{ended:?}");
    assert!(sender.send_request(req()).await.is_err());
    tokio::time::timeout(TIMEOUT, f.shared.drained())
        .await
        .expect("no connection left");
}
