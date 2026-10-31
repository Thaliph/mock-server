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
    let _logs = common::capture_logs();
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
async fn prx_13_pending_http1_request_is_answered_then_closed_with_no_byte_added() {
    let _logs = common::capture_logs();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let response: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    let dest = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let gate = gate.clone();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            gate.notified().await;
            s.write_all(response).await.unwrap();
            let mut sink = Vec::new();
            let _ = s.read_to_end(&mut sink).await;
        });
        addr
    };
    let f = fixture(Direction::Inbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nhost: svc\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.shared.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.notify_one();
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut got))
        .await
        .expect("the connection closes after the response")
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(response)
    );
    tokio::time::timeout(TIMEOUT, f.shared.drained())
        .await
        .expect("no connection left");
}

#[tokio::test]
async fn prx_13_idle_http1_connection_is_closed() {
    let _logs = common::capture_logs();
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
    let _logs = common::capture_logs();
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

// ---- PRX-20: the end of the grace closes what is still open ----

/// A destination that hands each accepted socket to the test, in order.
async fn handing_destination() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            let _ = tx.send(s);
        }
    });
    (addr, rx)
}

/// True when the peer closed (EOF or error) within `TIMEOUT`.
async fn sees_closed(s: &mut TcpStream) -> bool {
    let mut buf = [0u8; 64];
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn prx_20_grace_expiry_closes_tunnels_detections_and_pending_exchanges() {
    let logs = common::capture_logs();
    let (dest, mut accepted) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;

    // An opaque tunnel, carrying bytes.
    let mut tunnel = TcpStream::connect(f.addr).await.unwrap();
    tunnel.write_all(b"\x00\x01hello").await.unwrap();
    let mut tunnel_dest = accepted.recv().await.unwrap();
    let mut buf = [0u8; 7];
    tunnel_dest.read_exact(&mut buf).await.unwrap();

    // A connection still in detection.
    let mut detecting = TcpStream::connect(f.addr).await.unwrap();
    detecting.write_all(b"GE").await.unwrap();
    let mut detecting_dest = accepted.recv().await.unwrap();

    // An HTTP/1 exchange whose destination never answers.
    let mut pending = TcpStream::connect(f.addr).await.unwrap();
    pending
        .write_all(b"GET /slow HTTP/1.1\r\nhost: svc\r\n\r\n")
        .await
        .unwrap();
    let mut pending_dest = accepted.recv().await.unwrap();
    let mut head = [0u8; 64];
    let n = pending_dest.read(&mut head).await.unwrap();
    assert!(head[..n].starts_with(b"GET /slow"));

    let start = std::time::Instant::now();
    f.shared.shutdown();
    let shared = f.shared.clone();
    let drain = tokio::spawn(async move { shared.drain(Duration::from_secs(1)).await });

    // At 0.5 s the tunnel still carries bytes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    tunnel.write_all(b"ping").await.unwrap();
    let mut ping = [0u8; 4];
    tunnel_dest.read_exact(&mut ping).await.unwrap();
    assert_eq!(&ping, b"ping");
    tunnel_dest.write_all(b"pong").await.unwrap();
    tunnel.read_exact(&mut ping).await.unwrap();
    assert_eq!(&ping, b"pong");

    // After the grace, both sides of each see the connection closed.
    assert_eq!(drain.await.unwrap(), 3);
    assert!(
        start.elapsed() < Duration::from_millis(2500),
        "{:?}",
        start.elapsed()
    );
    for s in [
        &mut tunnel,
        &mut tunnel_dest,
        &mut detecting,
        &mut detecting_dest,
        &mut pending,
        &mut pending_dest,
    ] {
        assert!(sees_closed(s).await);
    }

    let lines = logs.access_exactly(3).await;
    for line in &lines {
        assert_eq!(line["result"], "shutdown", "{line}");
    }
    let warns: Vec<_> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["message"] == "grace period over")
        .collect();
    assert_eq!(warns.len(), 1, "{}", logs.text());
    assert_eq!(warns[0]["level"], "WARN");
    assert_eq!(warns[0]["connections"], 3);
}

#[tokio::test]
async fn prx_20_drain_returns_zero_when_everything_ended_in_time() {
    let _logs = common::capture_logs();
    let echo = common::echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"\x00\x01hi").await.unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await.unwrap();
    f.shared.shutdown();
    let shared = f.shared.clone();
    let drain = tokio::spawn(async move { shared.drain(Duration::from_secs(2)).await });
    drop(c);
    assert_eq!(drain.await.unwrap(), 0);
}
