//! MCK-14: non-HTTP traffic and passthrough in mock (spec §8), in process. The destination
//! is a listener that counts the connections it accepts and records what it receives. Every
//! wait has a deadline.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::mock_rig::{Dest, Options, Rig, TlsSetup, rig_with, users_set};
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST: &str = "GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f];

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..250 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

fn decision(rig: &Rig, decision: &str) -> u64 {
    rig.counter(&format!(
        "mimic_mock_connections_total{{decision=\"{decision}\"}}"
    ))
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

fn discovery() -> Discovery {
    Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap()
}

fn outbound_scopes(discovery: &Discovery) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_str(&discovery.report().to_json().unwrap()).unwrap();
    json["scopes"]["outbound"].clone()
}

#[tokio::test]
async fn mck_14_a_client_first_non_http_stream_reaches_its_destination_as_a_tcp_tunnel() {
    let discovery = discovery();
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            dest: Dest::Hold {
                greeting: b"",
                reply: b"S",
            },
            discovery: Some(discovery.tap()),
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(&SSL_REQUEST).await.unwrap();
    let mut answer = [0u8; 1];
    tokio::time::timeout(TIMEOUT, tcp.read_exact(&mut answer))
        .await
        .expect("relayed in time")
        .unwrap();
    assert_eq!(&answer, b"S");
    assert_eq!(*rig.received.lock().unwrap(), SSL_REQUEST);
    assert_eq!(decision(&rig, "tunnel"), 1);
    assert_eq!(decision(&rig, "mocked"), 0);
    eventually("the tcp tunnel is recorded", || {
        outbound_scopes(&discovery)
            .as_object()
            .is_some_and(|scopes| scopes.values().any(|s| s["tunnels"]["tcp"] == 1))
    })
    .await;
}

#[tokio::test]
async fn mck_14_a_server_first_destination_is_connected_after_one_second_of_silence() {
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            dest: Dest::Hold {
                greeting: b"5.7.0-mysql\n",
                reply: b"",
            },
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    let started = Instant::now();
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    let mut greeting = [0u8; 12];
    tokio::time::timeout(TIMEOUT, tcp.read_exact(&mut greeting))
        .await
        .expect("greeting in time")
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(&greeting, b"5.7.0-mysql\n");
    assert!(
        waited >= Duration::from_secs(1) && waited <= Duration::from_millis(1500),
        "{waited:?}"
    );
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(decision(&rig, "tunnel"), 1);
}

#[tokio::test]
async fn mck_14_an_http_request_after_one_second_to_a_silent_destination_is_mocked() {
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            dest: Dest::Hold {
                greeting: b"",
                reply: b"",
            },
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1, "connected at 1 s");
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let answer = read_response(&mut tcp).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.to_ascii_lowercase().contains("x-mimic-match"));
    eventually("the destination sees its socket closed", || {
        rig.closed.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);
    assert!(rig.received.lock().unwrap().is_empty(), "bytes reached it");
    assert_eq!(decision(&rig, "mocked"), 1);
    assert_eq!(decision(&rig, "tunnel"), 0);
}

#[tokio::test]
async fn mck_14_passthrough_http_reaches_the_destination_and_is_recorded() {
    let discovery = discovery();
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            passthrough: "users.test",
            dest: Dest::Hold {
                greeting: b"",
                reply:
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: text/plain\r\n\r\nok",
            },
            discovery: Some(discovery.tap()),
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let answer = read_response(&mut tcp).await;
    assert!(answer.ends_with("\r\n\r\nok"), "{answer}");
    assert!(!answer.to_ascii_lowercase().contains("x-mimic-match"));
    assert_eq!(*rig.received.lock().unwrap(), REQUEST.as_bytes());
    assert_eq!(decision(&rig, "passthrough"), 1);
    assert_eq!(decision(&rig, "mocked"), 0);
    eventually("the exchange is recorded", || {
        outbound_scopes(&discovery).to_string().contains("/users/")
    })
    .await;
}

#[tokio::test]
async fn mck_14_other_authorities_stay_mocked_with_a_passthrough_list() {
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            passthrough: "other.test",
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(REQUEST.as_bytes()).await.unwrap();
    let answer = read_response(&mut tcp).await;
    assert!(
        answer.to_ascii_lowercase().contains("x-mimic-match"),
        "{answer}"
    );
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    assert_eq!(decision(&rig, "mocked"), 1);
    assert_eq!(decision(&rig, "passthrough"), 0);
}

/// A TLS ClientHello offering TLS 1.1 at most, with this SNI (the sidecar cannot intercept
/// it, so it is relayed untouched).
fn hello(sni: &str) -> Vec<u8> {
    let mut ext = Vec::new();
    let host = sni.as_bytes();
    let mut list = vec![0u8];
    list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    list.extend_from_slice(host);
    let mut data = (list.len() as u16).to_be_bytes().to_vec();
    data.extend_from_slice(&list);
    ext.extend_from_slice(&[0, 0]);
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

#[tokio::test]
async fn mck_14_passthrough_tls_reaches_the_destination_and_is_recorded() {
    let discovery = discovery();
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            tls: Some(TlsSetup {
                exclude: "",
                ca: None,
            }),
            passthrough: "users.test",
            dest: Dest::Hold {
                greeting: b"",
                reply: b"",
            },
            discovery: Some(discovery.tap()),
            ..Options::default()
        },
    )
    .await;
    let client_hello = hello("users.test");
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(&client_hello).await.unwrap();
    eventually("the destination receives the hello", || {
        *rig.received.lock().unwrap() == client_hello
    })
    .await;
    assert_eq!(decision(&rig, "passthrough"), 1);
    assert_eq!(decision(&rig, "closed"), 0);
    eventually("the tunnel is recorded", || {
        outbound_scopes(&discovery)
            .to_string()
            .contains("tls_no_ca")
    })
    .await;
}

#[tokio::test]
async fn mck_14_a_passthrough_ip_entry_matches_the_destination_of_an_h2c_connection() {
    // The destination is 127.0.0.1: an IP entry passes an h2c connection through, whose
    // authority is not read.
    let rig = rig_with(
        users_set(5),
        100,
        1024,
        None,
        Options {
            passthrough: "127.0.0.1",
            dest: Dest::Hold {
                greeting: b"",
                reply: b"",
            },
            ..Options::default()
        },
    )
    .await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    tcp.write_all(preface).await.unwrap();
    // The relay speaks h2 to the destination itself, as in discovery.
    eventually("the destination receives a preface", || {
        rig.received.lock().unwrap().starts_with(preface)
    })
    .await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(decision(&rig, "passthrough"), 1);
}
