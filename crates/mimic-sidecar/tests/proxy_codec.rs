//! PRX-15, PRX-17, PRX-18, PRX-24, PRX-29 (integration), PRX-30: the byte-exact HTTP/1 relay
//! (`mimic_sidecar::http1`), driven directly over loopback sockets.

use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mimic_core::proxy::http1::{Event, Reason};
use mimic_sidecar::access::Outcome;
use mimic_sidecar::http1::{Relayed, relay};
use socket2::SockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(5);

/// One relayed connection: the address clients connect to, the events the codec reported, and
/// the relay's result.
struct Proxy {
    addr: SocketAddr,
    events: Arc<Mutex<Vec<Event>>>,
    done: JoinHandle<Relayed>,
}

impl Proxy {
    async fn finished(self) -> (Relayed, Vec<Event>) {
        let relayed = tokio::time::timeout(Duration::from_secs(2), self.done)
            .await
            .expect("relay ended in time")
            .unwrap();
        let events = self.events.lock().unwrap().clone();
        (relayed, events)
    }
}

async fn proxy(dest: SocketAddr) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let done = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut upstream = TcpStream::connect(dest).await.unwrap();
        relay(
            &mut client,
            &mut upstream,
            Vec::new(),
            move |e| sink.lock().unwrap().push(e),
            pending(),
            pending(),
        )
        .await
    });
    Proxy { addr, events, done }
}

async fn read_until(s: &mut TcpStream, want: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    while !got.ends_with(want) {
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(TIMEOUT, s.read(&mut buf))
            .await
            .expect("read in time")
            .unwrap();
        assert!(n > 0, "closed after {:?}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    got
}

async fn read_to_end(s: &mut TcpStream) -> Vec<u8> {
    let mut got = Vec::new();
    tokio::time::timeout(TIMEOUT, s.read_to_end(&mut got))
        .await
        .expect("closed in time")
        .unwrap();
    got
}

fn reset(s: TcpStream) {
    SockRef::from(&s).set_linger(Some(Duration::ZERO)).unwrap();
    drop(s);
}

async fn read_error(s: &mut TcpStream) -> io::ErrorKind {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(1), s.read(&mut buf)).await {
        Ok(Ok(0)) => panic!("FIN instead of a reset"),
        Ok(Ok(_)) => panic!("unexpected bytes"),
        Ok(Err(e)) => e.kind(),
        Err(_) => panic!("no reset within 1 s"),
    }
}

/// After a FIN, a reset shows as `ECONNRESET` on read or, the socket being in `CLOSE_WAIT`,
/// `EPIPE` on write (spec PRX-18).
async fn write_error(s: &mut TcpStream) -> io::ErrorKind {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if let Err(e) = s.write_all(b"x").await {
            return e.kind();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no reset within 1 s");
}

async fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

fn responses(events: &[Event]) -> Vec<u16> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Response(r) => Some(r.status),
            _ => None,
        })
        .collect()
}

fn unframed(events: &[Event]) -> Vec<Reason> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Unframed(u) => Some(u.reason),
            _ => None,
        })
        .collect()
}

// ---- PRX-15 -------------------------------------------------------------------------------

/// A conversation: steps of (bytes the client sends, bytes the destination answers once it
/// has read everything sent so far), and the final statuses the codec must report.
struct Conversation {
    name: &'static str,
    steps: Vec<(Vec<u8>, Vec<u8>)>,
    statuses: Vec<u16>,
}

fn step(req: &str, resp: &str) -> (Vec<u8>, Vec<u8>) {
    (req.as_bytes().to_vec(), resp.as_bytes().to_vec())
}

fn corpus() -> Vec<Conversation> {
    let many_headers: String = (0..200).map(|i| format!("X-H{i}: v{i}\r\n")).collect();
    let pipelined_req: String = (0..5)
        .map(|i| format!("GET /p{i} HTTP/1.1\r\nHost: a\r\n\r\n"))
        .collect();
    let pipelined_resp: String = (0..5)
        .map(|i| format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{i}!"))
        .collect();
    vec![
        Conversation {
            name: "headers: case, repeats, order, 200 response lines",
            steps: vec![step(
                "GET /a?q=1 HTTP/1.1\r\nHOST: a\r\nx-B: 1\r\nX-a: 2\r\nx-b: 3\r\nX-A:4\r\n\r\n",
                &format!("HTTP/1.1 200 OK\r\nContent-length: 2\r\n{many_headers}\r\nok"),
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "obs-fold in a non-framing header",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\nX-Fold: one\r\n  two\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX-Fold: a\r\n\tb\r\n\r\n",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "bare LF line ends",
            steps: vec![step(
                "GET / HTTP/1.1\nHost: a\n\n",
                "HTTP/1.1 200 OK\nContent-Length: 1\n\nx",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "chunked with extensions and trailers",
            steps: vec![step(
                "POST /c HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n0\r\nX-Trail: 1\r\n\r\n",
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n4;a=b\r\nwxyz\r\n0\r\nX-Sum: 9\r\nx-LOW: 1\r\n\r\n",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "close-delimited response",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                "HTTP/1.1 200 OK\r\n\r\nuntil the destination closes",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HTTP/1.0 client with keep-alive",
            steps: vec![step(
                "GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HTTP/1.0 client without keep-alive",
            steps: vec![step(
                "GET / HTTP/1.0\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "Expect: 100-continue",
            steps: vec![
                step(
                    "POST / HTTP/1.1\r\nHost: a\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n",
                    "HTTP/1.1 100 Continue\r\n\r\n",
                ),
                step("body", "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n"),
            ],
            statuses: vec![201],
        },
        Conversation {
            name: "103 before 200",
            steps: vec![step(
                "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                "HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n!",
            )],
            statuses: vec![200],
        },
        Conversation {
            name: "HEAD, 204, 304",
            steps: vec![
                step(
                    "HEAD / HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
                ),
                step(
                    "GET /n HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 204 No Content\r\nContent-Length: 7\r\n\r\n",
                ),
                step(
                    "GET /m HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 304 Not Modified\r\nContent-Length: 9\r\n\r\n",
                ),
            ],
            statuses: vec![200, 204, 304],
        },
        Conversation {
            name: "5 pipelined requests in one write",
            steps: vec![step(&pipelined_req, &pipelined_resp)],
            statuses: vec![200; 5],
        },
        Conversation {
            name: "response while no request is pending",
            steps: vec![
                step(
                    "GET / HTTP/1.1\r\nHost: a\r\n\r\n",
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                ),
                step(
                    "",
                    "HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n",
                ),
            ],
            statuses: vec![200, 408],
        },
    ]
}

#[tokio::test]
async fn prx_15_every_byte_is_relayed_unchanged_in_both_directions() {
    for conversation in corpus() {
        let name = conversation.name;
        let (l, dest) = listener().await;
        let p = proxy(dest).await;
        let steps = conversation.steps.clone();
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut received = Vec::new();
            let mut expected = 0;
            for (req, resp) in steps {
                expected += req.len();
                while received.len() < expected {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap();
                    assert!(n > 0, "destination: client closed early");
                    received.extend_from_slice(&buf[..n]);
                }
                s.write_all(&resp).await.unwrap();
            }
            // Close: ends a close-delimited response.
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
            received.extend_from_slice(&rest);
            received
        });
        let mut c = TcpStream::connect(p.addr).await.unwrap();
        let sent: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(req, _)| req.clone())
            .collect();
        for (req, _) in &conversation.steps {
            if !req.is_empty() {
                c.write_all(req).await.unwrap();
            }
        }
        let got = read_to_end(&mut c).await;
        let answered: Vec<u8> = conversation
            .steps
            .iter()
            .flat_map(|(_, resp)| resp.clone())
            .collect();
        assert_eq!(got, answered, "{name}: client side");
        drop(c);
        let received = tokio::time::timeout(TIMEOUT, server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, sent, "{name}: destination side");
        let (_, events) = p.finished().await;
        assert_eq!(responses(&events), conversation.statuses, "{name}: events");
        assert!(unframed(&events).is_empty(), "{name}: unframed");
    }
}

// ---- PRX-17 -------------------------------------------------------------------------------

#[tokio::test]
async fn prx_17_client_fin_reaches_the_destination_after_the_request_and_the_answer_comes_back() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let started = Instant::now();
        let got = read_to_end(&mut s).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        got
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let req = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc";
    c.write_all(req).await.unwrap();
    c.shutdown().await.unwrap();
    assert_eq!(
        read_to_end(&mut c).await,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
    );
    assert_eq!(server.await.unwrap(), req);
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::Ok));
}

#[tokio::test]
async fn prx_17_pipelined_requests_then_fin_are_all_read_before_eof() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let got = read_to_end(&mut s).await;
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        got
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let reqs = b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\nGET /2 HTTP/1.1\r\nHost: a\r\n\r\n";
    c.write_all(reqs).await.unwrap();
    c.shutdown().await.unwrap();
    let got = read_to_end(&mut c).await;
    assert_eq!(got.windows(6).filter(|w| w == b"200 OK").count(), 2);
    assert_eq!(server.await.unwrap(), reqs);
}

#[tokio::test]
async fn prx_17_destination_fin_after_a_close_delimited_response_follows_the_last_byte() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
        s.write_all(b"HTTP/1.1 200 OK\r\n\r\nhello").await.unwrap();
        s.shutdown().await.unwrap();
        let _ = read_to_end(&mut s).await;
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_to_end(&mut c).await, b"HTTP/1.1 200 OK\r\n\r\nhello");
}

#[tokio::test]
async fn prx_17_destination_closing_without_answering_after_the_client_eof_is_a_client_abort() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_to_end(&mut s).await;
        // Dropped: FIN with no answer.
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    c.shutdown().await.unwrap();
    assert!(read_to_end(&mut c).await.is_empty());
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::ClientAbort));
    assert_eq!(relayed.unanswered, 1);
}

#[tokio::test]
async fn prx_17_destination_closing_without_answering_before_any_client_eof_is_an_upstream_reset() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(
        tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    drop(c);
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::UpstreamReset));
}

// ---- PRX-18 -------------------------------------------------------------------------------

#[tokio::test]
async fn prx_18_client_reset_while_the_request_waits_resets_the_destination() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
        read_error(&mut s).await
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    reset(c);
    assert_eq!(server.await.unwrap(), io::ErrorKind::ConnectionReset);
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::ClientAbort));
}

#[tokio::test]
async fn prx_18_client_reset_after_a_fin_resets_the_destination() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let got = read_until(&mut s, b"\r\n\r\n").await;
        assert!(got.starts_with(b"GET"));
        let mut buf = [0u8; 16];
        assert_eq!(s.read(&mut buf).await.unwrap(), 0);
        write_error(&mut s).await
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    c.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    reset(c);
    let kind = server.await.unwrap();
    assert!(matches!(
        kind,
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    ));
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::ClientAbort));
}

#[tokio::test]
async fn prx_18_destination_reset_before_answering_resets_the_client() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
        reset(s);
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(read_error(&mut c).await, io::ErrorKind::ConnectionReset);
    let (relayed, _) = p.finished().await;
    assert!(matches!(relayed.outcome, Outcome::UpstreamReset));
}

// ---- PRX-24 -------------------------------------------------------------------------------

const WS: &str = "GET /ws HTTP/1.1\r\nHost: a\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
const CONNECT: &str = "CONNECT b:443 HTTP/1.1\r\nHost: b:443\r\n\r\n";
const LOOKS_LIKE_HTTP: &[u8] = b"GET /fake HTTP/1.1\r\nHost: a\r\n\r\n";

/// Request `head` plus `EXTRA` in one write; the destination checks that `EXTRA` only arrives
/// after it answered.
async fn upgrade_accepted(head: &str, accept: &'static [u8]) {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
        let mut buf = [0u8; 16];
        let early = tokio::time::timeout(Duration::from_millis(300), s.read(&mut buf)).await;
        assert!(
            early.is_err(),
            "bytes after the request arrived before the response"
        );
        s.write_all(accept).await.unwrap();
        let mut got = Vec::new();
        while got.len() < 5 + LOOKS_LIKE_HTTP.len() {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        s.write_all(LOOKS_LIKE_HTTP).await.unwrap();
        got
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(format!("{head}EXTRA").as_bytes())
        .await
        .unwrap();
    read_until(&mut c, b"\r\n\r\n").await;
    c.write_all(LOOKS_LIKE_HTTP).await.unwrap();
    read_until(&mut c, LOOKS_LIKE_HTTP).await;
    let got = server.await.unwrap();
    assert_eq!(got, [&b"EXTRA"[..], LOOKS_LIKE_HTTP].concat());
    drop(c);
    let (_, events) = p.finished().await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Request(_)))
            .count(),
        1
    );
    assert!(events.contains(&Event::Tunnel));
    assert_eq!(responses(&events).len(), 1);
}

#[tokio::test]
async fn prx_24_websocket_101_continues_as_an_unparsed_tunnel() {
    upgrade_accepted(
        WS,
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
    )
    .await;
}

#[tokio::test]
async fn prx_24_connect_200_continues_as_an_unparsed_tunnel() {
    upgrade_accepted(CONNECT, b"HTTP/1.1 200 Connection established\r\n\r\n").await;
}

async fn upgrade_declined(head: &str, decline: &'static [u8], status: u16) {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        read_until(&mut s, b"\r\n\r\n").await;
        s.write_all(decline).await.unwrap();
        let next = read_until(&mut s, b"\r\n\r\n").await;
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(s);
        next
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    // The next request is sent right behind the upgrade, before its answer.
    c.write_all(format!("{head}GET /next HTTP/1.1\r\nHost: a\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let started = Instant::now();
    let got = read_to_end(&mut c).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        got.ends_with(b"\r\n\r\nok"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    assert_eq!(
        server.await.unwrap(),
        b"GET /next HTTP/1.1\r\nHost: a\r\n\r\n"
    );
    drop(c);
    let (_, events) = p.finished().await;
    assert_eq!(responses(&events), vec![status, 200]);
    assert!(!events.contains(&Event::Tunnel));
}

#[tokio::test]
async fn prx_24_upgrade_declined_with_400_leaves_a_plain_connection() {
    upgrade_declined(
        WS,
        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        400,
    )
    .await;
}

#[tokio::test]
async fn prx_24_connect_declined_with_403_leaves_a_plain_connection() {
    upgrade_declined(
        CONNECT,
        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        403,
    )
    .await;
}

// ---- PRX-29 (integration) -------------------------------------------------------------------

/// The client sends `bad`; the destination answers; both are relayed unchanged, one
/// `unframed` event, and later HTTP-looking bytes produce no event.
async fn unframed_continues_as_a_tunnel(bad: Vec<u8>, reason: Reason) {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
    let want = bad.len() + LOOKS_LIKE_HTTP.len();
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut got = Vec::new();
        while got.len() < want {
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        s.write_all(answer).await.unwrap();
        s.write_all(LOOKS_LIKE_HTTP).await.unwrap();
        got
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(&bad).await.unwrap();
    c.write_all(LOOKS_LIKE_HTTP).await.unwrap();
    let got = read_until(&mut c, LOOKS_LIKE_HTTP).await;
    assert_eq!(got, [&answer[..], LOOKS_LIKE_HTTP].concat());
    assert_eq!(server.await.unwrap(), [&bad[..], LOOKS_LIKE_HTTP].concat());
    drop(c);
    let (_, events) = p.finished().await;
    assert_eq!(unframed(&events), vec![reason]);
    assert!(responses(&events).is_empty());
}

#[tokio::test]
async fn prx_29_content_length_with_transfer_encoding_continues_as_a_tunnel() {
    unframed_continues_as_a_tunnel(
        b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\nabcd"
            .to_vec(),
        Reason::ConflictingLength,
    )
    .await;
}

#[tokio::test]
async fn prx_29_header_line_over_64_kib_continues_as_a_tunnel() {
    let mut bad = b"GET / HTTP/1.1\r\nX: ".to_vec();
    bad.extend(std::iter::repeat_n(b'a', 70 * 1024));
    bad.extend_from_slice(b"\r\n\r\n");
    unframed_continues_as_a_tunnel(bad, Reason::HeadTooLarge).await;
}

#[tokio::test]
async fn prx_29_head_of_300_lines_continues_as_a_tunnel() {
    let mut bad = String::from("GET / HTTP/1.1\r\n");
    for i in 0..300 {
        bad.push_str(&format!("X{i}: v\r\n"));
    }
    bad.push_str("\r\n");
    unframed_continues_as_a_tunnel(bad.into_bytes(), Reason::TooManyHeaderLines).await;
}

// ---- PRX-30 -------------------------------------------------------------------------------

#[tokio::test]
async fn prx_30_only_32_pipelined_requests_are_ahead_of_their_responses() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let (go_tx, mut go_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    // The destination counts requests as they arrive and answers on demand.
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut seen = 0usize;
        let mut answered = 0usize;
        let mut acc = Vec::new();
        loop {
            let mut buf = [0u8; 8192];
            tokio::select! {
                n = s.read(&mut buf) => {
                    let n = n.unwrap_or(0);
                    if n == 0 { return; }
                    acc.extend_from_slice(&buf[..n]);
                    while let Some(p) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                        acc.drain(..p + 4);
                        seen += 1;
                    }
                    tx.send(seen).unwrap();
                }
                Some(k) = go_rx.recv() => {
                    for _ in 0..k {
                        s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{answered:03}").as_bytes()).await.unwrap();
                        answered += 1;
                    }
                }
            }
        }
    });
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    let all: String = (0..100)
        .map(|i| format!("GET /{i} HTTP/1.1\r\nHost: a\r\n\r\n"))
        .collect();
    let (mut r, mut w) = c.split();
    let mut seen = 0;
    let write = w.write_all(all.as_bytes());
    let settle = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        while let Ok(n) = rx.try_recv() {
            seen = n;
        }
        seen
    };
    let (wrote, seen_first) = tokio::join!(write, settle);
    wrote.unwrap();
    assert_eq!(seen_first, 32);
    go_tx.send(1).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(n) = rx.try_recv() {
        seen = n;
    }
    assert_eq!(seen, 33);
    go_tx.send(99).unwrap();
    let mut got = Vec::new();
    while got.len() < 100 * 41 {
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(TIMEOUT, r.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let expected: String = (0..100)
        .map(|i| format!("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{i:03}"))
        .collect();
    assert_eq!(String::from_utf8(got).unwrap(), expected);
}

#[tokio::test]
async fn prx_30_a_10_mib_header_line_reaches_the_destination_unchanged() {
    let (l, dest) = listener().await;
    let p = proxy(dest).await;
    const LEN: usize = 10 * 1024 * 1024;
    let server = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut got = Vec::with_capacity(LEN);
        let mut buf = vec![0u8; 64 * 1024];
        while got.len() < LEN {
            let k = s.read(&mut buf).await.unwrap();
            assert!(k > 0);
            got.extend_from_slice(&buf[..k]);
        }
        got
    });
    let mut data = b"GET / HTTP/1.1\r\nX: ".to_vec();
    data.resize(LEN, b'a');
    let mut c = TcpStream::connect(p.addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), c.write_all(&data))
        .await
        .unwrap()
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .unwrap()
        .unwrap();
    assert!(got == data, "bytes differ");
    drop(c);
    let (_, events) = p.finished().await;
    assert_eq!(unframed(&events), vec![Reason::HeadTooLarge]);
}
