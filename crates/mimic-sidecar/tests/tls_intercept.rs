//! TLS-17, TLS-22 (decrypted path) and TLS-23 (intercepted part), in process: a rustls
//! destination whose CA is in `trust/0/image-ca.pem`, the sidecar's outbound listener with an
//! intermediate installed, and a client that trusts the deployment root only.

mod common;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::red::ProxyMetrics;
use mimic_sidecar::tls::ca::{CaMetrics, CaState};
use mimic_sidecar::tls::intercept::{Interception, TlsMetrics};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

const TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST: &[u8] = b"GET /a?q=canary-query-91 HTTP/1.1\r\nHost: api.test:443\r\nX-Canary: canary-header-91\r\nCookie: s=canary-cookie-91\r\nContent-Length: 0\r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

type Authority = Issuer<'static, KeyPair>;

fn authority(name: &str) -> (Authority, CertificateDer<'static>) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, name.to_string());
    let key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (Issuer::new(params, key), cert.der().clone())
}

fn pem(der: &CertificateDer<'_>) -> String {
    let b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(der.as_ref())
    };
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        lines.join("\n")
    )
}

/// The deployment root and a `CaState` holding an intermediate under it.
fn ca_state() -> (Arc<CaState>, CertificateDer<'static>) {
    let (root, root_der) = authority("mimic root");
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "mimic intermediate".to_string());
    let cert = params.signed_by(&key, &root).unwrap();
    let state = Arc::new(CaState::new(CaMetrics::default()).unwrap());
    state
        .install_intermediate(root_der.as_ref(), key, &cert.pem())
        .unwrap();
    (state, root_der)
}

struct Destination {
    addr: SocketAddr,
    /// `trust/0/image-ca.pem` of the destination's CA.
    trust: tempfile::TempDir,
    /// SNI seen in each handshake.
    sni: Arc<Mutex<Vec<Option<String>>>>,
    /// Request bytes read (HTTP/1) per connection.
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// A TLS destination for `api.test` / 127.0.0.1 speaking `alpn`; HTTP/1 connections answer
/// `RESPONSE` once the request head and then the client's end of stream were read, h2
/// connections answer `GET <path>`.
async fn destination(alpn: &[&[u8]]) -> Destination {
    let (ca, ca_der) = authority("destination ca");
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::DnsName("api.test".try_into().unwrap()),
        SanType::IpAddress("127.0.0.1".parse().unwrap()),
    ];
    let cert = params.signed_by(&key, &ca).unwrap();
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sni: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let (seen, read) = (sni.clone(), requests.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, seen, read) = (acceptor.clone(), seen.clone(), read.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                seen.lock()
                    .unwrap()
                    .push(tls.get_ref().1.server_name().map(str::to_string));
                if tls.get_ref().1.alpn_protocol() == Some(&b"h2"[..]) {
                    let service = service_fn(|req: Request<hyper::body::Incoming>| async move {
                        let body = format!("{} {}", req.method(), req.uri().path());
                        Ok::<_, io::Error>(Response::new(Full::new(Bytes::from(body))))
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                    return;
                }
                let mut got = Vec::new();
                let mut buf = [0u8; 1024];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                // A request that asks for it is answered only once the client's close_notify
                // + FIN, which comes after the request, was read.
                if !got.windows(8).any(|w| w == b"wait-eof") {
                    let _ = tls.write_all(RESPONSE).await;
                }
                let mut rest = Vec::new();
                let _ = tls.read_to_end(&mut rest).await;
                got.extend_from_slice(&rest);
                read.lock().unwrap().push(got.clone());
                if got.windows(8).any(|w| w == b"wait-eof") {
                    let _ = tls.write_all(RESPONSE).await;
                }
                let _ = tls.shutdown().await;
            });
        }
    });
    let trust = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(trust.path().join("trust/0")).unwrap();
    std::fs::write(trust.path().join("trust/0/image-ca.pem"), pem(&ca_der)).unwrap();
    Destination {
        addr,
        trust,
        sni,
        requests,
    }
}

fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

struct Sidecar {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
}

async fn sidecar(
    dest: &Destination,
    ca: Option<Arc<CaState>>,
    exclusions: &str,
) -> Sidecar {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let tls = TlsMetrics::register(&mut metrics);
    let interception = Interception::new(
        Exclusions::parse(exclusions).unwrap(),
        ca,
        Vec::<PathBuf>::new(),
        dest.trust.path().to_path_buf(),
        tls,
    );
    let to = dest.addr;
    let original: OriginalDst = Arc::new(move |_| Ok(to));
    let shared = Arc::new(
        Shared::new(vec![addr], tunnel, original)
            .with_proxy_metrics(proxy)
            .with_tls(interception),
    );
    spawn_listener(listener, Direction::Outbound, shared);
    Sidecar {
        addr,
        metrics: Arc::new(metrics),
    }
}

fn client_config(root: &CertificateDer<'static>, alpn: &[&[u8]]) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    TlsConnector::from(Arc::new(config))
}

async fn connect(
    sidecar: &Sidecar,
    root: &CertificateDer<'static>,
    name: ServerName<'static>,
    alpn: &[&[u8]],
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(sidecar.addr).await?;
    tokio::time::timeout(TIMEOUT, client_config(root, alpn).connect(name, tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake"))?
}

fn api() -> ServerName<'static> {
    ServerName::try_from("api.test").unwrap()
}

async fn exchange(
    tls: &mut tokio_rustls::client::TlsStream<TcpStream>,
    request: &[u8],
) -> Vec<u8> {
    tls.write_all(request).await.unwrap();
    let mut out = vec![0u8; RESPONSE.len()];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut out))
        .await
        .expect("answer in time")
        .unwrap();
    out
}

#[tokio::test]
async fn tls_17_a_client_gets_a_leaf_from_the_intermediate_and_the_destination_sees_the_sni() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let chain = tls.get_ref().1.peer_certificates().unwrap().to_vec();
    let (_, leaf) = X509Certificate::from_der(chain[0].as_ref()).unwrap();
    assert!(leaf.issuer().to_string().contains("mimic intermediate"));
    let san = leaf.subject_alternative_name().unwrap().unwrap();
    assert!(
        san.value
            .general_names
            .iter()
            .any(|n| matches!(n, GeneralName::DNSName("api.test")))
    );
    assert!(leaf.validity().is_valid());
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    tls.shutdown().await.unwrap();
    assert_eq!(*dest.sni.lock().unwrap(), vec![Some("api.test".to_string())]);
    // The destination saw the request unchanged.
    for _ in 0..100 {
        if !dest.requests.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(dest.requests.lock().unwrap()[0], REQUEST);
    // The exchange line: decrypted, with the SNI's authority (Host carries the default port).
    for _ in 0..100 {
        if !logs.access().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lines = logs.access();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["tls"], true);
    assert_eq!(lines[0]["protocol"], "http1");
    assert_eq!(lines[0]["authority"], "api.test");
    assert_eq!(lines[0]["status"], 200);
    assert_eq!(lines[0]["direction"], "outbound");
    // TLS-23: the canaries of the PRX-08 corpus appear in no line.
    let text = logs.text();
    assert!(!text.contains("canary-"), "{text}");
    common::counted(
        &sidecar.metrics,
        "mimic_proxy_connections_total{direction=\"outbound\",protocol=\"http1\",authority=\"api.test\"} 1",
    )
    .await;
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"intercepted\"} 1",
    )
    .await;
}

#[tokio::test]
async fn tls_17_h2_follows_the_destinations_alpn_and_a_client_without_alpn_gets_http1() {
    let dest = destination(&[b"h2", b"http/1.1"]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let tls = connect(&sidecar, &root, api(), &[b"h2", b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, Empty<Bytes>>(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(conn);
    for path in ["/one", "/two"] {
        let req = Request::get(format!("https://api.test{path}"))
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
    common::counted(
        &sidecar.metrics,
        "mimic_proxy_connections_total{direction=\"outbound\",protocol=\"h2\",authority=\"api.test\"} 1",
    )
    .await;
    // No ALPN offered: none negotiated, HTTP/1.1 spoken.
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
}

#[tokio::test]
async fn tls_17_without_sni_the_leaf_names_the_destination_ip_and_leaves_are_reused() {
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let ip = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connect(&sidecar, &root, ip, &[]).await.unwrap();
    let chain = tls.get_ref().1.peer_certificates().unwrap().to_vec();
    let (_, leaf) = X509Certificate::from_der(chain[0].as_ref()).unwrap();
    let san = leaf.subject_alternative_name().unwrap().unwrap();
    assert!(san.value.general_names.iter().any(
        |n| matches!(n, GeneralName::IPAddress(b) if *b == [127, 0, 0, 1])
    ));
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    assert_eq!(*dest.sni.lock().unwrap(), vec![None]);
    // Two connections with the same SNI get the same leaf.
    let one = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let two = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let der = |t: &tokio_rustls::client::TlsStream<TcpStream>| {
        t.get_ref().1.peer_certificates().unwrap()[0].as_ref().to_vec()
    };
    assert_eq!(der(&one), der(&two));
}

#[tokio::test]
async fn tls_22_the_clients_close_notify_reaches_the_destination_after_the_request() {
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let request = b"GET /b HTTP/1.1\r\nHost: api.test\r\nX-Mode: wait-eof\r\n\r\n";
    tls.write_all(request).await.unwrap();
    // close_notify + FIN right after the request; the destination can still answer.
    tls.shutdown().await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut out))
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(out, RESPONSE);
    assert_eq!(dest.requests.lock().unwrap()[0], request);
}

#[tokio::test]
async fn tls_23_connections_are_counted_under_their_decision() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "*.excluded.test").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    // Passed through: the client does not get the mimic leaf (and here rejects the
    // destination's certificate, which is not for this name).
    let excluded = ServerName::try_from("a.excluded.test").unwrap();
    let _ = connect(&sidecar, &root, excluded, &[]).await;
    let _ = connect(&sidecar, &root, api(), &[b"imap"]).await;
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"intercepted\"} 1",
    )
    .await;
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"excluded\"} 1",
    )
    .await;
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"unsupported\"} 1",
    )
    .await;
    // Passed-through TLS stays under `tls`, with its reason on the connection line.
    common::counted(
        &sidecar.metrics,
        "mimic_proxy_connections_total{direction=\"outbound\",protocol=\"tls\",authority=\"a.excluded.test\"} 1",
    )
    .await;
    for _ in 0..100 {
        if logs
            .access()
            .iter()
            .any(|l| l["interception"] == "excluded")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lines = logs.access();
    let excluded = lines.iter().find(|l| l["interception"] == "excluded").unwrap();
    assert_eq!(excluded["protocol"], "tls");
    assert_eq!(excluded["sni"], "a.excluded.test");
    assert!(lines.iter().any(|l| l["interception"] == "unsupported"));
    // No CA: passed through.
    let none = self::sidecar(&dest, None, "").await;
    let _ = connect(&none, &root, api(), &[]).await;
    common::counted(
        &none.metrics,
        "mimic_tls_connections_total{interception=\"no_ca\"} 1",
    )
    .await;
}
