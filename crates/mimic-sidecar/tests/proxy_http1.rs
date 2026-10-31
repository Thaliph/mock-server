//! PRX-03, PRX-06, PRX-07 (HTTP/1 part), PRX-11 (HTTP/1 part): the HTTP/1 relay, in process.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{closed_by_peer, closed_port, fixture};
use mimic_sidecar::listener::Direction;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(5);

/// What the scripted destination saw and how it was reached.
struct Dest {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    received: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
}

/// A destination that, for each request (head ending with `\r\n\r\n` plus `body_len` bytes),
/// records the raw bytes and writes the next canned response.
async fn scripted(responses: Vec<&'static [u8]>, body_len: usize) -> Dest {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let (tx, received) = tokio::sync::mpsc::unbounded_channel();
    let count = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            let responses = responses.clone();
            tokio::spawn(async move {
                let mut pending = Vec::new();
                for response in responses {
                    let want = loop {
                        if let Some(p) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4 + body_len;
                        }
                        let mut buf = [0u8; 4096];
                        match s.read(&mut buf).await {
                            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    };
                    while pending.len() < want {
                        let mut buf = [0u8; 4096];
                        match s.read(&mut buf).await {
                            Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    let rest = pending.split_off(want);
                    tx.send(std::mem::replace(&mut pending, rest)).unwrap();
                    s.write_all(response).await.unwrap();
                }
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink).await;
            });
        }
    });
    Dest {
        addr,
        connections,
        received,
    }
}

async fn read_until(c: &mut TcpStream, total: usize) -> Vec<u8> {
    let mut out = vec![0u8; total];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut out))
        .await
        .expect("read in time")
        .expect("read");
    out
}

async fn recv(d: &mut Dest) -> Vec<u8> {
    tokio::time::timeout(TIMEOUT, d.received.recv())
        .await
        .expect("destination saw a request")
        .unwrap()
}

const DIRECTIONS: [Direction; 2] = [Direction::Inbound, Direction::Outbound];

#[tokio::test]
async fn prx_03_request_and_response_reach_each_side_byte_for_byte() {
    let request: &[u8] = b"POST /a/b?x=1&y=%20z HTTP/1.1\r\nHOST: svc.example\r\nX-Mixed-Case: One\r\nuser-agent: t\r\nAccept: */*\r\nX-Dup: 1\r\nX-Dup: 2\r\nContent-Length: 11\r\n\r\nhello world";
    let response: &[u8] = b"HTTP/1.1 200 All Good\r\nContent-Type: text/plain\r\nX-Powered-By: nobody\r\nContent-Length: 5\r\n\r\nhowdy";
    for direction in DIRECTIONS {
        let mut dest = scripted(vec![response], 11).await;
        let addr = dest.addr;
        let f = fixture(direction, move |_| Some(addr)).await;
        let mut c = TcpStream::connect(f.addr).await.unwrap();
        c.write_all(request).await.unwrap();
        assert_eq!(recv(&mut dest).await, request);
        assert_eq!(read_until(&mut c, response.len()).await, response);
    }
}

#[tokio::test]
async fn prx_03_chunked_bodies_and_trailers_pass_through() {
    let request: &[u8] = b"POST /up HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nTrailer: X-Sum\r\n\r\n5\r\nhello\r\n0\r\nX-Sum: abc\r\n\r\n";
    let response: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Done\r\n\r\n3\r\nyes\r\n0\r\nX-Done: 1\r\n\r\n";
    // The scripted destination stops at the end of the head: the body is read from the
    // connection by the assertion on the response only, so give it the whole request.
    let mut dest = scripted(vec![response], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(request).await.unwrap();
    let head = recv(&mut dest).await;
    assert!(head.starts_with(
        b"POST /up HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nTrailer: X-Sum\r\n\r\n"
    ));
    assert_eq!(read_until(&mut c, response.len()).await, response);
}

#[tokio::test]
async fn prx_03_keep_alive_and_pipelined_requests_use_one_destination_connection() {
    let r1: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n1";
    let r2: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n2";
    let r3: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n3";
    let mut dest = scripted(vec![r1, r2, r3], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    // Two pipelined in one write, then a third later.
    c.write_all(b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\nGET /2 HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_until(&mut c, r1.len() * 2).await, [r1, r2].concat());
    c.write_all(b"GET /3 HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_until(&mut c, r3.len()).await, r3);
    for path in ["/1", "/2", "/3"] {
        let seen = recv(&mut dest).await;
        assert!(seen.starts_with(format!("GET {path} HTTP/1.1").as_bytes()));
    }
    assert_eq!(dest.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn prx_03_host_naming_another_server_still_goes_to_the_original_destination() {
    let response: &[u8] = b"HTTP/1.1 204 No Content\r\n\r\n";
    let mut dest = scripted(vec![response], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let request: &[u8] = b"GET http://elsewhere.example:9/x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n";
    c.write_all(request).await.unwrap();
    assert_eq!(recv(&mut dest).await, request);
    assert_eq!(read_until(&mut c, response.len()).await, response);
}

/// A destination that answers `reply` to the first request head, then echoes raw bytes.
async fn upgrading(reply: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            if s.read(&mut b).await.unwrap_or(0) == 0 {
                return;
            }
            head.push(b[0]);
        }
        s.write_all(reply).await.unwrap();
        let mut buf = [0u8; 1024];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    addr
}

#[tokio::test]
async fn prx_06_websocket_upgrade_carries_bytes_both_ways() {
    let reply: &[u8] =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    let addr = upgrading(reply).await;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(
        b"GET /ws HTTP/1.1\r\nHost: a\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    )
    .await
    .unwrap();
    assert_eq!(read_until(&mut c, reply.len()).await, reply);
    c.write_all(b"frame-one").await.unwrap();
    assert_eq!(read_until(&mut c, 9).await, b"frame-one");
    c.write_all(b"frame-two").await.unwrap();
    assert_eq!(read_until(&mut c, 9).await, b"frame-two");
}

#[tokio::test]
async fn prx_06_connect_answered_200_carries_bytes_both_ways() {
    let reply: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";
    let addr = upgrading(reply).await;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"CONNECT db.example:5432 HTTP/1.1\r\nHost: db.example:5432\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_until(&mut c, reply.len()).await, reply);
    c.write_all(b"\x00\x01binary").await.unwrap();
    assert_eq!(read_until(&mut c, 8).await, b"\x00\x01binary");
    // A half-close reaches the destination, which closes in turn.
    c.shutdown().await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut rest))
        .await
        .expect("closed")
        .unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn prx_07_refused_destination_closes_the_client_without_a_byte() {
    let dead = closed_port().await;
    let f = fixture(Direction::Outbound, move |_| Some(dead)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    closed_by_peer(c).await;
}

#[tokio::test]
async fn prx_07_destination_closing_after_the_request_closes_the_client_without_a_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        drop(s);
    });
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, c.read_to_end(&mut got))
        .await
        .expect("closed");
    assert!(got.is_empty(), "{:?}", String::from_utf8_lossy(&got));
}

async fn answer_to(request: Vec<u8>) -> Vec<u8> {
    let response: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let dest = scripted(vec![response], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    // The proxy may answer and close before the whole request is written.
    let _ = c.write_all(&request).await;
    let mut got = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, c.read_to_end(&mut got)).await;
    got
}

#[tokio::test]
async fn prx_07_malformed_header_gets_400() {
    let got = answer_to(b"GET / HTTP/1.1\r\nHost: a\r\nBad Header\r\n\r\n".to_vec()).await;
    assert!(
        got.starts_with(b"HTTP/1.1 400 "),
        "{:?}",
        String::from_utf8_lossy(&got)
    );
}

#[tokio::test]
async fn prx_07_more_than_256_header_lines_gets_431() {
    let mut req = b"GET / HTTP/1.1\r\nHost: a\r\n".to_vec();
    for i in 0..300 {
        req.extend_from_slice(format!("X-H{i}: v\r\n").as_bytes());
    }
    req.extend_from_slice(b"\r\n");
    let got = answer_to(req).await;
    assert!(
        got.starts_with(b"HTTP/1.1 431 "),
        "{:?}",
        String::from_utf8_lossy(&got)
    );
}

#[tokio::test]
async fn prx_07_header_section_over_64_kib_gets_431() {
    let mut req = b"GET / HTTP/1.1\r\nHost: a\r\n".to_vec();
    for i in 0..10 {
        req.extend_from_slice(format!("X-Big{i}: {}\r\n", "a".repeat(8000)).as_bytes());
    }
    req.extend_from_slice(b"\r\n");
    let got = answer_to(req).await;
    assert!(
        got.starts_with(b"HTTP/1.1 431 "),
        "{:?}",
        String::from_utf8_lossy(&got)
    );
}

#[tokio::test]
async fn prx_11_idle_keep_alive_connection_carries_the_next_request_after_35_seconds() {
    let r: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    let dest = scripted(vec![r, r], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_until(&mut c, r.len()).await, r);
    tokio::time::sleep(Duration::from_secs(35)).await;
    c.write_all(b"GET /2 HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_until(&mut c, r.len()).await, r);
    assert_eq!(dest.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn prx_11_client_half_close_after_a_request_still_gets_the_response() {
    let r: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
    let dest = scripted(vec![r], 0).await;
    let addr = dest.addr;
    let f = fixture(Direction::Inbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    c.shutdown().await.unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut got))
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(got, r);
}

#[tokio::test]
async fn prx_11_destination_closing_an_idle_keep_alive_connection_reaches_the_client_within_1s() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf).await;
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(s);
    });
    let f = fixture(Direction::Outbound, move |_| Some(addr)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    let r: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    assert_eq!(read_until(&mut c, r.len()).await, r);
    let start = Instant::now();
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf))
        .await
        .expect("closed")
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}
