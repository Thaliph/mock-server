//! MCK-16: live switching of open connections, both ways (spec §8 "Switching"), in process.
//! The mode is flipped the way a pull does (`ModeState::observe`). Every wait has a deadline.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::mock_rig::{AUTHORITY, Dest, Options, Rig, TlsSetup, rig_with, users_set};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::Mode;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::mode::Announced;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST: &str = "GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f];
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: text/plain\r\n\r\nok";

async fn switch(rig: &Rig, mode: Mode) {
    rig.mode
        .observe(Announced {
            mode: Ok(mode),
            factor: None,
        })
        .await;
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..250 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

/// Reads one response head and the body it announces.
async fn read_response(tcp: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(TIMEOUT, tcp.read(&mut chunk))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&out));
        out.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&out).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_string)
                })
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if body.len() >= length {
                return text;
            }
        }
    }
}

/// Reads until the peer closes with a FIN (an error, a reset, fails the test).
async fn closed_by_fin(tcp: &mut TcpStream) {
    let mut chunk = [0u8; 256];
    let n = tokio::time::timeout(TIMEOUT, tcp.read(&mut chunk))
        .await
        .expect("closed in time")
        .expect("a FIN, not a reset");
    assert_eq!(n, 0, "bytes before the FIN");
}

fn matched(answer: &str) -> bool {
    answer.to_ascii_lowercase().contains("x-mimic-match")
}

async fn forwarding_rig(options: Options) -> Rig {
    rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            mode: Some(Mode::Discovery),
            dest: Dest::Hold {
                greeting: b"",
                reply: OK,
            },
            ..options
        },
    )
    .await
}

// --- to mock ----------------------------------------------------------------------------

#[tokio::test]
async fn mck_16_http1_keep_alive_request_after_the_switch_is_mocked_and_the_destination_closed() {
    let rig = forwarding_rig(Options::default()).await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let first = read_response(&mut tcp).await;
    assert!(first.ends_with("ok") && !matched(&first), "{first}");
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);

    switch(&rig, Mode::Mock).await;
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let second = read_response(&mut tcp).await;
    assert!(
        second.starts_with("HTTP/1.1 200") && matched(&second),
        "{second}"
    );
    eventually("the destination sees its connection closed", || {
        rig.closed.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(*rig.received.lock().unwrap(), REQUEST.as_bytes());
    // The connection keeps being mocked.
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    assert!(matched(&read_response(&mut tcp).await));
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mck_16_a_passthrough_connection_keeps_forwarding_after_the_switch() {
    let rig = forwarding_rig(Options {
        passthrough: "users.test",
        ..Options::default()
    })
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let first = read_response(&mut tcp).await;
    assert!(first.ends_with("ok"), "{first}");
    switch(&rig, Mode::Mock).await;
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    eventually("the second request reaches the destination", || {
        rig.received.lock().unwrap().len() == 2 * REQUEST.len()
    })
    .await;
    assert_eq!(rig.closed.load(Ordering::SeqCst), 0);
}

/// An `h2c` destination that counts its connections, requests and ended connections.
struct H2Dest {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
}

async fn h2_destination() -> H2Dest {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dest = H2Dest {
        addr: listener.local_addr().unwrap(),
        connections: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(AtomicUsize::new(0)),
        ended: Arc::new(AtomicUsize::new(0)),
    };
    let (connections, requests, ended) = (
        dest.connections.clone(),
        dest.requests.clone(),
        dest.ended.clone(),
    );
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            connections.fetch_add(1, Ordering::SeqCst);
            let (requests, ended) = (requests.clone(), ended.clone());
            tokio::spawn(async move {
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                    requests.fetch_add(1, Ordering::SeqCst);
                    async {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                    }
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(tcp), service)
                    .await;
                ended.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    dest
}

async fn h2_get(
    sender: &mut hyper::client::conn::http2::SendRequest<Full<Bytes>>,
) -> Response<hyper::body::Incoming> {
    let request = Request::builder()
        .uri(format!("http://{AUTHORITY}/users/3"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    tokio::time::timeout(TIMEOUT, sender.send_request(request))
        .await
        .expect("answered in time")
        .expect("an answer")
}

#[tokio::test]
async fn mck_16_a_new_h2c_stream_after_the_switch_is_mocked_and_the_destination_closed() {
    let dest = h2_destination().await;
    let rig = forwarding_rig(Options {
        dst: Some(dest.addr),
        ..Options::default()
    })
    .await;
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    let first = h2_get(&mut sender).await;
    assert_eq!(first.status(), 200);
    assert!(first.headers().get("x-mimic-match").is_none());
    assert_eq!(first.into_body().collect().await.unwrap().to_bytes(), "ok");

    switch(&rig, Mode::Mock).await;
    let second = h2_get(&mut sender).await;
    assert_eq!(second.status(), 200);
    assert!(second.headers().get("x-mimic-match").is_some());
    eventually("the destination connection ended", || {
        dest.ended.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(dest.requests.load(Ordering::SeqCst), 1);
    // The client connection continues, mocked.
    let third = h2_get(&mut sender).await;
    assert!(third.headers().get("x-mimic-match").is_some());
    assert_eq!(dest.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mck_16_a_tls_tunnel_passed_through_is_closed_and_a_tcp_tunnel_keeps_flowing() {
    let rig = forwarding_rig(Options {
        tls: Some(TlsSetup {
            exclude: "users.test",
            ca: None,
        }),
        ..Options::default()
    })
    .await;
    let mut tls = TcpStream::connect(rig.addr).await.unwrap();
    tls.write_all(&client_hello("users.test")).await.unwrap();
    // The ClientHello is relayed to the destination, which answers the first bytes.
    let mut reply = vec![0u8; OK.len()];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut reply))
        .await
        .expect("relayed in time")
        .unwrap();
    // A plain TCP tunnel on the same rig destination, opened before the switch.
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(&SSL_REQUEST).await.unwrap();
    let mut answer = vec![0u8; OK.len()];
    tokio::time::timeout(TIMEOUT, tcp.read_exact(&mut answer))
        .await
        .expect("relayed in time")
        .unwrap();

    let switched = Instant::now();
    switch(&rig, Mode::Mock).await;
    let mut chunk = [0u8; 16];
    let ended = tokio::time::timeout(Duration::from_secs(1), tls.read(&mut chunk))
        .await
        .expect("closed within a second of the pull");
    assert!(matches!(ended, Ok(0) | Err(_)), "{ended:?}");
    assert!(switched.elapsed() < Duration::from_secs(1));
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["result"], "mock_closed", "{access:?}");

    // The non-HTTP tunnel still relays both ways.
    tcp.write_all(b"more").await.unwrap();
    eventually("the tunnel relays", || {
        rig.received.lock().unwrap().ends_with(b"more")
    })
    .await;
    let mut probe = [0u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), tcp.read(&mut probe))
            .await
            .is_err(),
        "the tcp tunnel was closed"
    );
}

/// A TLS ClientHello offering TLS 1.1 at most, with this SNI.
fn client_hello(sni: &str) -> Vec<u8> {
    let host = sni.as_bytes();
    let mut list = vec![0u8];
    list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    list.extend_from_slice(host);
    let mut data = (list.len() as u16).to_be_bytes().to_vec();
    data.extend_from_slice(&list);
    let mut ext = vec![0, 0];
    ext.extend_from_slice(&(data.len() as u16).to_be_bytes());
    ext.extend_from_slice(&data);
    let mut body = vec![0x03, 0x02];
    body.extend_from_slice(&[7u8; 32]);
    body.push(0);
    body.extend_from_slice(&[0, 2, 0x00, 0x2f]);
    body.extend_from_slice(&[1, 0]);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut handshake = vec![1, 0];
    handshake.extend_from_slice(&(body.len() as u16).to_be_bytes());
    handshake.extend_from_slice(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
    record.extend_from_slice(&handshake);
    record
}

// --- back to discovery --------------------------------------------------------------------

fn slow_set() -> mimic_core::discovery::egress::EgressSet {
    users_set(300)
}

#[tokio::test]
async fn mck_16_a_mocked_http1_connection_gets_its_pending_answer_then_a_fin() {
    let discovery = Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap();
    let rig = rig_with(
        slow_set(),
        100,
        1024,
        None,
        Options {
            dest: Dest::Hold {
                greeting: b"",
                reply: OK,
            },
            discovery: Some(discovery.tap()),
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    switch(&rig, Mode::Discovery).await;
    let answer = read_response(&mut tcp).await;
    assert!(matched(&answer), "{answer}");
    closed_by_fin(&mut tcp).await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);

    // The next connection reaches the destination and is recorded.
    let mut next = TcpStream::connect(rig.addr).await.unwrap();
    next.write_all(REQUEST.as_bytes()).await.unwrap();
    let answer = read_response(&mut next).await;
    assert!(answer.ends_with("ok") && !matched(&answer), "{answer}");
    assert_eq!(*rig.received.lock().unwrap(), REQUEST.as_bytes());
    eventually("the exchange is recorded", || {
        let json: serde_json::Value =
            serde_json::from_str(&discovery.report().to_json().unwrap()).unwrap();
        json["scopes"]["outbound"].to_string().contains("/users/")
    })
    .await;
}

#[tokio::test]
async fn mck_16_a_mocked_h2c_connection_gets_goaway_after_its_pending_stream() {
    let rig = rig_with(slow_set(), 100, 1024, None, Options::default()).await;
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    let driver = tokio::spawn(conn);
    let mut pending = sender.clone();
    let stream = tokio::spawn(async move { h2_get(&mut pending).await.status() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    switch(&rig, Mode::Discovery).await;
    assert_eq!(stream.await.unwrap(), 200);
    // GOAWAY: the connection ends, and no new stream is accepted.
    let ended = tokio::time::timeout(TIMEOUT, driver)
        .await
        .expect("the connection ended after GOAWAY")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
    let request = Request::builder()
        .uri(format!("http://{AUTHORITY}/users/3"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert!(sender.send_request(request).await.is_err());
}
