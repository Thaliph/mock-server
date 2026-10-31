//! TLS-22 (transport part, in process): the HTTP/1 codec relay and the h2 relay over TLS
//! streams. The decrypted bytes are the ones compared; a FIN is `close_notify` then FIN (and a
//! FIN without `close_notify` is a FIN); a TCP reset stays a reset.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::exchange::Ctx;
use mimic_sidecar::listener::Direction;
use mimic_sidecar::red::ProxyMetrics;
use mimic_sidecar::transport::{Socket, Stream};
use mimic_sidecar::{h2, http1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST: &[u8] = b"GET /a HTTP/1.1\r\nHost: dest.example\r\nX-Odd:  v \r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

struct Pki {
    acceptor: TlsAcceptor,
    connector: TlsConnector,
}

fn pki(alpn: &[&[u8]]) -> Pki {
    let cert = rcgen::generate_simple_self_signed(vec!["dest.example".to_string()]).unwrap();
    let der: CertificateDer<'static> = cert.cert.der().clone();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut server = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    server.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let mut roots = RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Pki {
        acceptor: TlsAcceptor::from(Arc::new(server)),
        connector: TlsConnector::from(Arc::new(client)),
    }
}

/// A connected TCP pair on loopback.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (a, b) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (a.unwrap(), b.unwrap().0)
}

/// The sidecar's two TLS streams of one connection and the TLS peers on their other ends.
struct Wired {
    client: Stream,
    upstream: Stream,
    app: tokio_rustls::client::TlsStream<TcpStream>,
    dest: tokio_rustls::server::TlsStream<TcpStream>,
}

async fn wired(alpn: &[&[u8]]) -> Wired {
    let pki = pki(alpn);
    let name = || ServerName::try_from("dest.example").unwrap();
    let (app_tcp, client_tcp) = pair().await;
    let (upstream_tcp, dest_tcp) = pair().await;
    let (app, client) = tokio::join!(
        pki.connector.connect(name(), app_tcp),
        pki.acceptor.accept(Socket::new(client_tcp))
    );
    let (upstream, dest) = tokio::join!(
        pki.connector.connect(name(), Socket::new(upstream_tcp)),
        pki.acceptor.accept(dest_tcp)
    );
    Wired {
        client: Stream::tls(client.unwrap().into()),
        upstream: Stream::tls(upstream.unwrap().into()),
        app: app.unwrap(),
        dest: dest.unwrap(),
    }
}

async fn read_exactly<S: AsyncRead + Unpin>(s: &mut S, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    tokio::time::timeout(TIMEOUT, s.read_exact(&mut out))
        .await
        .expect("read in time")
        .expect("read");
    out
}

fn spawn_relay(
    mut client: Stream,
    mut upstream: Stream,
) -> tokio::task::JoinHandle<http1::Relayed> {
    tokio::spawn(async move {
        let relayed = http1::relay(
            &mut client,
            &mut upstream,
            Vec::new(),
            |_| {},
            std::future::pending(),
            std::future::pending(),
        )
        .await;
        drop((client, upstream));
        relayed
    })
}

#[tokio::test]
async fn tls_22_http1_over_tls_relays_the_decrypted_bytes_unchanged() {
    let mut w = wired(&[]).await;
    let relay = spawn_relay(w.client, w.upstream);
    w.app.write_all(REQUEST).await.unwrap();
    assert_eq!(read_exactly(&mut w.dest, REQUEST.len()).await, REQUEST);
    w.dest.write_all(RESPONSE).await.unwrap();
    assert_eq!(read_exactly(&mut w.app, RESPONSE.len()).await, RESPONSE);
    w.app.shutdown().await.unwrap();
    let mut rest = Vec::new();
    w.dest.read_to_end(&mut rest).await.unwrap();
    w.dest.shutdown().await.unwrap();
    let relayed = relay.await.unwrap();
    assert!(rest.is_empty());
    assert_eq!(relayed.to_upstream, REQUEST.len() as u64);
    assert_eq!(relayed.to_client, RESPONSE.len() as u64);
    assert_eq!(relayed.unanswered, 0);
}

#[tokio::test]
async fn tls_22_close_notify_and_fin_reach_the_destination_as_close_notify_and_fin() {
    let mut w = wired(&[]).await;
    let relay = spawn_relay(w.client, w.upstream);
    w.app.write_all(REQUEST).await.unwrap();
    // close_notify, then FIN, right after a complete request.
    w.app.shutdown().await.unwrap();
    assert_eq!(read_exactly(&mut w.dest, REQUEST.len()).await, REQUEST);
    // The destination sees a clean end of stream (read_to_end fails without close_notify) ...
    let mut rest = Vec::new();
    tokio::time::timeout(TIMEOUT, w.dest.read_to_end(&mut rest))
        .await
        .expect("end of stream in time")
        .expect("clean close_notify");
    assert!(rest.is_empty());
    // ... and can still answer.
    w.dest.write_all(RESPONSE).await.unwrap();
    w.dest.shutdown().await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(TIMEOUT, w.app.read_to_end(&mut answer))
        .await
        .unwrap()
        .expect("clean close_notify towards the client");
    assert_eq!(answer, RESPONSE);
    relay.await.unwrap();
}

#[tokio::test]
async fn tls_22_a_fin_without_close_notify_is_a_fin_with_close_notify_on_the_other_side() {
    let mut w = wired(&[]).await;
    let relay = spawn_relay(w.client, w.upstream);
    w.app.write_all(REQUEST).await.unwrap();
    w.app.flush().await.unwrap();
    // The client closes its TCP connection without close_notify (what many clients do).
    let (tcp, _) = w.app.into_inner();
    socket2::SockRef::from(&tcp)
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    assert_eq!(read_exactly(&mut w.dest, REQUEST.len()).await, REQUEST);
    let mut rest = Vec::new();
    tokio::time::timeout(TIMEOUT, w.dest.read_to_end(&mut rest))
        .await
        .expect("end of stream in time")
        .expect("the destination gets close_notify, not a truncation");
    w.dest.shutdown().await.unwrap();
    relay.await.unwrap();
    drop(tcp);
}

#[tokio::test]
async fn tls_22_a_tcp_reset_reaches_the_other_side_as_a_reset() {
    for from_client in [true, false] {
        let mut w = wired(&[]).await;
        let relay = spawn_relay(w.client, w.upstream);
        w.app.write_all(REQUEST).await.unwrap();
        assert_eq!(read_exactly(&mut w.dest, REQUEST.len()).await, REQUEST);
        let started = std::time::Instant::now();
        let (reset, mut other): (_, Box<dyn AsyncRead + Unpin + Send>) = if from_client {
            (w.app.into_inner().0, Box::new(w.dest))
        } else {
            (w.dest.into_inner().0, Box::new(w.app))
        };
        socket2::SockRef::from(&reset)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(reset);
        let mut buf = Vec::new();
        let err = tokio::time::timeout(TIMEOUT, other.read_to_end(&mut buf))
            .await
            .expect("the reset arrives in time")
            .expect_err("a reset, not an end of stream");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
        assert!(started.elapsed() < Duration::from_secs(1));
        relay.await.unwrap();
    }
}

fn ctx(dst: std::net::SocketAddr) -> Ctx {
    let mut metrics = Metrics::new("sidecar", "test");
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let (_tx, aborted) = tokio::sync::watch::channel(false);
    Ctx {
        direction: Direction::Outbound,
        dst,
        tls: true,
        proxy,
        aborted,
    }
}

#[tokio::test]
async fn tls_22_h2_over_tls_relays_streams_and_keeps_the_alpn() {
    let w = wired(&[b"h2"]).await;
    assert_eq!(w.app.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    // The destination: one h2 server on its TLS stream.
    let dest = w.dest;
    tokio::spawn(async move {
        let service = service_fn(|req: Request<hyper::body::Incoming>| async move {
            let body = format!("{} {}", req.method(), req.uri().path());
            Ok::<_, io::Error>(Response::new(Full::new(Bytes::from(body))))
        });
        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(dest), service)
            .await;
    });
    let (client, upstream) = (w.client, w.upstream);
    let relay = tokio::spawn(async move {
        h2::relay(
            client,
            upstream,
            Vec::new(),
            ctx("127.0.0.1:443".parse().unwrap()),
            std::future::pending::<()>(),
        )
        .await
    });
    let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(w.app))
        .await
        .unwrap();
    tokio::spawn(conn);
    for path in ["/one", "/two"] {
        let req = Request::get(format!("https://dest.example{path}"))
            .body(Empty::new())
            .unwrap();
        let resp = tokio::time::timeout(TIMEOUT, sender.send_request(req))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, format!("GET {path}"));
    }
    drop(sender);
    let _ = tokio::time::timeout(TIMEOUT, relay).await;
}
