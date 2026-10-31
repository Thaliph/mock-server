//! PRX-05, PRX-11 (tunnel part), PRX-12 (non-log part): the connection layer for banner,
//! non-HTTP and TLS traffic, in process.

mod common;

use std::io;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{counted, echo_server, fixture};
use mimic_sidecar::connection::configure_socket;
use mimic_sidecar::listener::{Direction, raise_fd_limit};
use nix::sys::socket::{getsockopt, sockopt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};

const TIMEOUT: Duration = Duration::from_secs(5);

async fn read_exactly(c: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, c.read_exact(&mut buf))
        .await
        .expect("read in time")
        .expect("read");
    buf
}

/// A destination that sends `banner` as soon as a client connects, then echoes.
async fn banner_server(banner: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                if s.write_all(banner).await.is_err() {
                    return;
                }
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn prx_05_server_first_banner_reaches_a_silent_client() {
    let server = banner_server(b"220 smtp.example ESMTP\r\n").await;
    let f = fixture(Direction::Outbound, move |_| Some(server)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    assert_eq!(
        read_exactly(&mut c, 24).await,
        b"220 smtp.example ESMTP\r\n"
    );
    c.write_all(b"EHLO x\r\n").await.unwrap();
    assert_eq!(read_exactly(&mut c, 8).await, b"EHLO x\r\n");
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="outbound",result="ok"} 1"#,
    )
    .await;
}

#[tokio::test]
async fn prx_05_non_http_client_bytes_are_tunnelled_byte_for_byte() {
    let echo = echo_server().await;
    let f = fixture(Direction::Inbound, move |_| Some(echo)).await;
    // Neither a TLS record, an HTTP request line nor the HTTP/2 preface; includes NUL bytes.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let c = TcpStream::connect(f.addr).await.unwrap();
    let (mut rd, mut wr) = c.into_split();
    let sent = payload.clone();
    let writer = tokio::spawn(async move {
        wr.write_all(&sent).await.unwrap();
        wr
    });
    let mut got = vec![0u8; payload.len()];
    tokio::time::timeout(TIMEOUT, rd.read_exact(&mut got))
        .await
        .expect("echo in time")
        .unwrap();
    assert!(got == payload, "bytes differ");
    writer.await.unwrap();
}

#[tokio::test]
async fn prx_05_bytes_split_over_several_writes_before_detection_are_forwarded_unchanged() {
    let echo = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    // A TLS record header that never becomes a ClientHello still reaches the destination.
    for part in [&[0x16u8][..], &[0x03, 0x01][..], b"\x00\x05hello"] {
        c.write_all(part).await.unwrap();
        c.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(read_exactly(&mut c, 10).await, b"\x16\x03\x01\x00\x05hello");
}

#[tokio::test]
async fn prx_05_tls_handshake_completes_and_shows_the_destination_certificate() {
    let cert = rcgen::generate_simple_self_signed(vec!["dest.example".to_string()]).unwrap();
    let der = cert.cert.der().clone();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let server_config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let dest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dest_addr = dest.local_addr().unwrap();
    tokio::spawn(async move {
        let (tcp, _) = dest.accept().await.unwrap();
        let mut tls = acceptor.accept(tcp).await.unwrap();
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.unwrap();
        tls.write_all(&buf).await.unwrap();
        tls.flush().await.unwrap();
        let _ = tls.read(&mut buf).await;
    });

    let f = fixture(Direction::Outbound, move |_| Some(dest_addr)).await;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(der.clone()).unwrap();
    let client_config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    let mut tls = connector
        .connect(ServerName::try_from("dest.example").unwrap(), tcp)
        .await
        .expect("handshake through the sidecar");
    let seen = tls.get_ref().1.peer_certificates().unwrap();
    assert_eq!(seen[0].as_ref(), der.as_ref());
    tls.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"ping");
}

#[tokio::test]
async fn prx_11_idle_tunnel_carries_the_next_exchange_after_35_seconds() {
    let echo = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"one\n").await.unwrap();
    assert_eq!(read_exactly(&mut c, 4).await, b"one\n");
    tokio::time::sleep(Duration::from_secs(35)).await;
    c.write_all(b"two\n").await.unwrap();
    assert_eq!(read_exactly(&mut c, 4).await, b"two\n");
}

#[tokio::test]
async fn prx_11_client_half_close_reaches_the_destination_which_can_still_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dest = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        s.read_to_end(&mut request).await.unwrap();
        s.write_all(&request).await.unwrap();
        s.write_all(b" and goodbye").await.unwrap();
    });
    let f = fixture(Direction::Inbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"request\n").await.unwrap();
    c.shutdown().await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut answer))
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(answer, b"request\n and goodbye");
}

#[tokio::test]
async fn prx_11_destination_closing_an_idle_connection_reaches_the_client_within_1s() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dest = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).await.unwrap();
        s.write_all(&buf).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(s);
    });
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"ping\n").await.unwrap();
    assert_eq!(read_exactly(&mut c, 5).await, b"ping\n");
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

#[tokio::test]
async fn prx_11_configure_socket_sets_keepalive_300_30_4() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stream = TcpStream::connect(addr).await.unwrap();
    configure_socket(&stream).unwrap();
    assert!(getsockopt(&stream, sockopt::KeepAlive).unwrap());
    assert_eq!(getsockopt(&stream, sockopt::TcpKeepIdle).unwrap(), 300);
    assert_eq!(getsockopt(&stream, sockopt::TcpKeepInterval).unwrap(), 30);
    assert_eq!(getsockopt(&stream, sockopt::TcpKeepCount).unwrap(), 4);
}

/// Seconds left on the keepalive timer of the first socket for which `select(local, peer)`
/// holds, as `ss` reports it; `None` when there is no such socket or it has no timer.
fn keepalive_timer(select: impl Fn(&str, &str) -> bool) -> Option<u64> {
    let out = Command::new("ss").args(["-tnoH"]).output().unwrap();
    let out = String::from_utf8(out.stdout).unwrap();
    let line = out.lines().find(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        cols.len() > 4 && select(cols[3], cols[4])
    })?;
    let timer = line.split("timer:(keepalive,").nth(1)?.split(',').next()?;
    let mut secs = 0;
    let mut number = String::new();
    for ch in timer.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
        } else if ch == 'm' {
            secs += number.parse::<u64>().ok()? * 60;
            number.clear();
        } else if ch == 's' {
            secs += number.parse::<u64>().ok()?;
            number.clear();
        }
    }
    Some(secs)
}

#[tokio::test]
async fn prx_11_both_sockets_of_a_tunnel_have_keepalive_set() {
    let echo = echo_server().await;
    let f = fixture(Direction::Outbound, move |_| Some(echo)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"x\n").await.unwrap();
    read_exactly(&mut c, 2).await;
    let (listener, client, echo) = (
        f.addr.to_string(),
        c.local_addr().unwrap().to_string(),
        echo.to_string(),
    );
    // Client side of the sidecar: local = listener address, peer = the test client.
    let client_side = keepalive_timer(|l, p| l == listener && p == client)
        .expect("keepalive on the accepted socket");
    assert!((200..=300).contains(&client_side), "{client_side}");
    // Destination side: the sidecar's socket towards the echo server (ephemeral local port).
    let upstream = keepalive_timer(|l, p| p == echo && l != echo)
        .expect("keepalive on the destination socket");
    assert!((200..=300).contains(&upstream), "{upstream}");
}

#[test]
fn prx_12_raise_fd_limit_makes_the_soft_limit_equal_to_the_hard_limit() {
    raise_fd_limit();
    let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap();
    let cols: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(cols[3], cols[4], "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prx_12_a_thousand_simultaneous_connections_are_all_served() {
    raise_fd_limit();
    let echo = echo_server().await;
    let f = fixture(Direction::Inbound, move |_| Some(echo)).await;
    let mut clients = Vec::new();
    for _ in 0..1000 {
        clients.push(TcpStream::connect(f.addr).await.unwrap());
    }
    for (i, c) in clients.iter_mut().enumerate() {
        c.write_all(format!("{i:04}\n").as_bytes()).await.unwrap();
    }
    for (i, c) in clients.iter_mut().enumerate() {
        assert_eq!(read_exactly(c, 5).await, format!("{i:04}\n").as_bytes());
    }
    counted(
        &f.metrics,
        r#"mimic_tcp_connections_total{direction="inbound",result="ok"} 1000"#,
    )
    .await;
}

// ---- PRX-17 / PRX-18 (tunnel part): FIN passes as FIN, reset passes as reset ----

fn reset(s: TcpStream) {
    socket2::SockRef::from(&s)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(s);
}

/// Waits for the failure that shows a reset (or fails the test after `TIMEOUT`).
async fn expect_reset(s: &mut TcpStream) -> Duration {
    let start = Instant::now();
    tokio::time::timeout(TIMEOUT, async {
        let mut buf = [0u8; 64];
        loop {
            match s.read(&mut buf).await {
                Ok(0) => {
                    // After a FIN the reset only shows on the error readiness, and Linux
                    // reports a reset in CLOSE_WAIT as EPIPE.
                    s.ready(Interest::ERROR).await.unwrap();
                    let e = s.take_error().unwrap().expect("a pending socket error");
                    assert_eq!(e.kind(), io::ErrorKind::BrokenPipe, "{e}");
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e.kind(), io::ErrorKind::ConnectionReset, "{e}");
                    return;
                }
            }
        }
    })
    .await
    .expect("reset in time");
    start.elapsed()
}

/// A destination that hands its accepted socket to the test.
async fn handing_destination() -> (SocketAddr, tokio::sync::oneshot::Receiver<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let _ = tx.send(s);
    });
    (addr, rx)
}

#[tokio::test]
async fn prx_18_opaque_client_reset_reaches_the_destination_as_a_reset() {
    let (dest, rx) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"hello\n").await.unwrap();
    let mut d = rx.await.unwrap();
    read_exactly(&mut d, 6).await;
    reset(c);
    let took = expect_reset(&mut d).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn prx_18_opaque_client_reset_after_a_fin_reaches_the_destination_as_a_reset() {
    let (dest, rx) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"hello\n").await.unwrap();
    c.shutdown().await.unwrap();
    let mut d = rx.await.unwrap();
    let mut buf = [0u8; 6];
    tokio::time::timeout(TIMEOUT, d.read_exact(&mut buf))
        .await
        .expect("bytes in time")
        .unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, d.read(&mut buf))
            .await
            .expect("fin in time")
            .unwrap(),
        0
    );
    reset(c);
    let took = expect_reset(&mut d).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn prx_18_opaque_destination_reset_reaches_the_client_as_a_reset() {
    let (dest, rx) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"hello\n").await.unwrap();
    let mut d = rx.await.unwrap();
    read_exactly(&mut d, 6).await;
    reset(d);
    let took = expect_reset(&mut c).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn prx_18_opaque_destination_reset_after_a_fin_reaches_the_client_as_a_reset() {
    let (dest, rx) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"hello\n").await.unwrap();
    let mut d = rx.await.unwrap();
    read_exactly(&mut d, 6).await;
    d.shutdown().await.unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(TIMEOUT, c.read(&mut buf))
            .await
            .expect("fin in time")
            .unwrap(),
        0
    );
    reset(d);
    let took = expect_reset(&mut c).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn prx_17_opaque_destination_fin_passes_as_a_fin_and_not_as_a_reset() {
    let (dest, rx) = handing_destination().await;
    let f = fixture(Direction::Outbound, move |_| Some(dest)).await;
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    c.write_all(b"hello\n").await.unwrap();
    let mut d = rx.await.unwrap();
    d.write_all(b"bye\n").await.unwrap();
    d.shutdown().await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(TIMEOUT, c.read_to_end(&mut answer))
        .await
        .expect("fin in time")
        .expect("a FIN, not a reset");
    assert_eq!(answer, b"bye\n");
}

/// A TLS destination and a client that completed the handshake through the sidecar.
async fn tls_pair() -> (
    tokio_rustls::client::TlsStream<TcpStream>,
    tokio_rustls::server::TlsStream<TcpStream>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["dest.example".to_string()]).unwrap();
    let der = cert.cert.der().clone();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let server_config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let dest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dest_addr = dest.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = dest.accept().await.unwrap();
        acceptor.accept(tcp).await.unwrap()
    });
    let f = fixture(Direction::Outbound, move |_| Some(dest_addr)).await;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let client_config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(f.addr).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(client_config))
        .connect(ServerName::try_from("dest.example").unwrap(), tcp)
        .await
        .expect("handshake through the sidecar");
    (tls, server.await.unwrap())
}

#[tokio::test]
async fn prx_18_tls_client_reset_reaches_the_destination_as_a_reset() {
    let (tls, mut d) = tls_pair().await;
    let (tcp, _) = tls.into_inner();
    reset(tcp);
    let took = expect_reset(d.get_mut().0).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn prx_18_tls_destination_reset_reaches_the_client_as_a_reset() {
    let (mut tls, d) = tls_pair().await;
    let (tcp, _) = d.into_inner();
    reset(tcp);
    let took = expect_reset(tls.get_mut().0).await;
    assert!(took < Duration::from_secs(1), "{took:?}");
}
