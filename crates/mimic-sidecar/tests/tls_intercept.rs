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
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

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
    /// TCP connections accepted, handshake or not.
    accepts: Arc<std::sync::atomic::AtomicUsize>,
    /// Whether each completed handshake carried a client certificate.
    client_certs: Arc<Mutex<Vec<bool>>>,
    /// How each HTTP/1 connection ended after its request: `Ok` for an end of stream.
    closes: Arc<Mutex<Vec<Result<(), io::ErrorKind>>>>,
    /// The destination's CA, for clients that trust it.
    ca_der: CertificateDer<'static>,
}

/// A TLS destination for `api.test` / 127.0.0.1 speaking `alpn`; HTTP/1 connections answer
/// `RESPONSE` once the request head and then the client's end of stream were read, h2
/// connections answer `GET <path>`.
async fn destination(alpn: &[&[u8]]) -> Destination {
    destination_with(alpn, true, None).await
}

/// `trusted`: its CA is in the image trust store; `client_ca`: client certificates signed by
/// it are requested (not required).
async fn destination_with(
    alpn: &[&[u8]],
    trusted: bool,
    client_ca: Option<&CertificateDer<'static>>,
) -> Destination {
    let (ca, ca_der) = authority("destination ca");
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::DnsName("api.test".try_into().unwrap()),
        SanType::IpAddress("127.0.0.1".parse().unwrap()),
    ];
    let cert = params.signed_by(&key, &ca).unwrap();
    let builder = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = match client_ca {
        None => builder.with_no_client_auth(),
        Some(der) => {
            let mut roots = RootCertStore::empty();
            roots.add(der.clone()).unwrap();
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
                .allow_unauthenticated()
                .build()
                .unwrap();
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder
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
    let accepts: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
    let client_certs: Arc<Mutex<Vec<bool>>> = Arc::default();
    let closes: Arc<Mutex<Vec<Result<(), io::ErrorKind>>>> = Arc::default();
    let (seen, read, count, certs, ended) = (
        sni.clone(),
        requests.clone(),
        accepts.clone(),
        client_certs.clone(),
        closes.clone(),
    );
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (acceptor, seen, read) = (acceptor.clone(), seen.clone(), read.clone());
            let (certs, ended) = (certs.clone(), ended.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                seen.lock()
                    .unwrap()
                    .push(tls.get_ref().1.server_name().map(str::to_string));
                certs
                    .lock()
                    .unwrap()
                    .push(tls.get_ref().1.peer_certificates().is_some());
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
                if got.windows(7).any(|w| w == b"X-Mode:") {
                    if got.windows(4).any(|w| w == b"hang") {
                        std::future::pending::<()>().await;
                    }
                    if got.windows(5).any(|w| w == b"reset") {
                        let (tcp, _) = tls.into_inner();
                        let _ = socket2::SockRef::from(&tcp).set_linger(Some(Duration::ZERO));
                        return;
                    }
                }
                // A request that asks for it is answered only once the client's close_notify
                // + FIN, which comes after the request, was read.
                if !got.windows(8).any(|w| w == b"wait-eof") {
                    let _ = tls.write_all(RESPONSE).await;
                }
                let mut rest = Vec::new();
                let closed = tls.read_to_end(&mut rest).await;
                ended
                    .lock()
                    .unwrap()
                    .push(closed.map(|_| ()).map_err(|e| e.kind()));
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
    if trusted {
        std::fs::write(trust.path().join("trust/0/image-ca.pem"), pem(&ca_der)).unwrap();
    }
    Destination {
        addr,
        trust,
        sni,
        requests,
        accepts,
        client_certs,
        closes,
        ca_der,
    }
}

fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

struct Sidecar {
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    shared: Arc<Shared>,
}

async fn sidecar(dest: &Destination, ca: Option<Arc<CaState>>, exclusions: &str) -> Sidecar {
    sidecar_in(dest, ca, exclusions, Direction::Outbound).await
}

async fn sidecar_in(
    dest: &Destination,
    ca: Option<Arc<CaState>>,
    exclusions: &str,
    direction: Direction,
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
    spawn_listener(listener, direction, shared.clone());
    Sidecar {
        addr,
        metrics: Arc::new(metrics),
        shared,
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

async fn exchange(tls: &mut tokio_rustls::client::TlsStream<TcpStream>, request: &[u8]) -> Vec<u8> {
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
    // Strict verifiers (Python 3.13) need the leaf's AKI to name the intermediate's SKI.
    let (_, intermediate) = X509Certificate::from_der(chain[1].as_ref()).unwrap();
    let ski = intermediate
        .extensions()
        .iter()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        });
    let aki = leaf
        .extensions()
        .iter()
        .find_map(|e| match e.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|id| id.0.to_vec())
            }
            _ => None,
        });
    assert!(ski.is_some(), "intermediate without Subject Key Identifier");
    assert_eq!(aki, ski, "leaf Authority Key Identifier");
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    tls.shutdown().await.unwrap();
    assert_eq!(
        *dest.sni.lock().unwrap(),
        vec![Some("api.test".to_string())]
    );
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
    assert!(
        san.value
            .general_names
            .iter()
            .any(|n| matches!(n, GeneralName::IPAddress(b) if *b == [127, 0, 0, 1]))
    );
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    assert_eq!(*dest.sni.lock().unwrap(), vec![None]);
    // Two connections with the same SNI get the same leaf.
    let one = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let two = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let der = |t: &tokio_rustls::client::TlsStream<TcpStream>| {
        t.get_ref().1.peer_certificates().unwrap()[0]
            .as_ref()
            .to_vec()
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
        let lines = logs.access();
        if ["excluded", "unsupported"]
            .iter()
            .all(|r| lines.iter().any(|l| l["interception"] == *r))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lines = logs.access();
    let excluded = lines
        .iter()
        .find(|l| l["interception"] == "excluded")
        .unwrap();
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

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..150 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never: {what}");
}

fn warns(logs: &common::Logs) -> usize {
    logs.text().matches("tls passthrough").count()
}

/// The client trusts the destination's CA only: what it sees through a pass-through.
async fn own_view(
    sidecar: &Sidecar,
    dest: &Destination,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut tls = connect(sidecar, &dest.ca_der, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    let chain = tls.get_ref().1.peer_certificates().unwrap().to_vec();
    let (_, leaf) = X509Certificate::from_der(chain[0].as_ref()).unwrap();
    assert!(!leaf.issuer().to_string().contains("mimic"));
    tls
}

#[tokio::test]
async fn tls_18_an_unverifiable_destination_is_passed_through_and_remembered() {
    let logs = common::capture_logs();
    let dest = destination_with(&[], false, None).await;
    let (ca, _root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let one = own_view(&sidecar, &dest).await;
    // The destination got the client's ClientHello byte for byte: its handshake is the
    // client's own, with the client's SNI.
    assert_eq!(
        *dest.sni.lock().unwrap(),
        vec![Some("api.test".to_string())]
    );
    let two = own_view(&sidecar, &dest).await;
    drop((one, two));
    // Sidecar attempt + replay for the first connection, replay only for the second.
    assert_eq!(dest.accepts.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(warns(&logs), 1, "{}", logs.text());
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"upstream_untrusted\"} 2",
    )
    .await;
    common::counted(&sidecar.metrics, "mimic_tls_passthrough_destinations 1").await;
    eventually("two connection lines", || {
        logs.access()
            .iter()
            .filter(|l| l["interception"] == "upstream_untrusted" && l["protocol"] == "tls")
            .count()
            == 2
    })
    .await;
}

#[tokio::test]
async fn tls_18_a_destination_that_resets_during_the_handshake_resets_the_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = accepts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Read the ClientHello, then reset.
            let mut tcp = tcp;
            let mut buf = [0u8; 4096];
            let _ = tcp.read(&mut buf).await;
            let _ = socket2::SockRef::from(&tcp).set_linger(Some(Duration::ZERO));
        }
    });
    let mut dest = destination(&[]).await;
    dest.addr = addr;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    for _ in 0..2 {
        let started = std::time::Instant::now();
        assert!(connect(&sidecar, &root, api(), &[]).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    // Not remembered: the sidecar tried again.
    assert_eq!(accepts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        sidecar
            .metrics
            .encode()
            .contains("mimic_tls_passthrough_destinations 0")
    );
}

#[tokio::test]
async fn tls_19_a_destination_asking_for_a_client_certificate_gets_the_clients() {
    let logs = common::capture_logs();
    let (client_ca, client_ca_der) = authority("client ca");
    let dest = destination_with(&[], true, Some(&client_ca_der)).await;
    let (ca, _root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::default()
        .signed_by(&key, &client_ca)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(dest.ca_der.clone()).unwrap();
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let tcp = TcpStream::connect(sidecar.addr).await.unwrap();
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(api(), tcp)
        .await
        .unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    // The destination saw the client's certificate on the connection that completed.
    assert!(dest.client_certs.lock().unwrap().contains(&true));
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"client_cert_requested\"} 1",
    )
    .await;
    common::counted(&sidecar.metrics, "mimic_tls_passthrough_destinations 1").await;
    assert_eq!(warns(&logs), 1);
}

#[tokio::test]
async fn tls_20_a_client_that_rejects_the_mimic_leaf_gets_the_destination_next_time() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, _root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    // Trusts the destination's CA, not the mimic root.
    assert!(connect(&sidecar, &dest.ca_der, api(), &[]).await.is_err());
    eventually("remembered", || warns(&logs) == 1).await;
    let tls = own_view(&sidecar, &dest).await;
    drop(tls);
    assert_eq!(warns(&logs), 1);
    assert!(logs.text().contains("client_rejected"));
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"client_rejected\"} 2",
    )
    .await;
    common::counted(&sidecar.metrics, "mimic_tls_passthrough_destinations 1").await;
}

#[tokio::test]
async fn tls_21_excluded_unsupported_and_inbound_connections_are_not_intercepted() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca.clone()), "*.excluded.test").await;
    let excluded = ServerName::try_from("a.excluded.test").unwrap();
    let _ = connect(&sidecar, &root, excluded, &[]).await;
    // Without any handshake from the sidecar: the destination only saw the client's.
    assert_eq!(dest.accepts.load(std::sync::atomic::Ordering::SeqCst), 1);
    // ALPN `imap` only: unsupported, and not remembered.
    for _ in 0..2 {
        let _ = connect(&sidecar, &root, api(), &[b"imap"]).await;
    }
    common::counted(
        &sidecar.metrics,
        "mimic_tls_connections_total{interception=\"unsupported\"} 2",
    )
    .await;
    common::counted(&sidecar.metrics, "mimic_tls_passthrough_destinations 0").await;
    assert_eq!(warns(&logs), 0);
    // Intercepted still.
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    // Inbound: never intercepted, no `interception` on its line.
    let inbound = sidecar_in(&dest, Some(ca), "", Direction::Inbound).await;
    let _ = connect(&inbound, &root, api(), &[]).await;
    eventually("inbound line", || {
        logs.access()
            .iter()
            .any(|l| l["direction"] == "inbound" && l["protocol"] == "tls")
    })
    .await;
    let lines = logs.access();
    let line = lines.iter().find(|l| l["direction"] == "inbound").unwrap();
    assert!(line.get("interception").is_none());
}

#[tokio::test]
async fn tls_22_a_client_reset_reaches_the_destination_and_the_reverse() {
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    let (tcp, _) = tls.into_inner();
    let _ = socket2::SockRef::from(&tcp).set_linger(Some(Duration::ZERO));
    drop(tcp);
    eventually("reset reaches the destination", || {
        matches!(dest.closes.lock().unwrap().first(), Some(Err(_)))
    })
    .await;
    // The destination resets its side: the client sees a reset within 1 s.
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    tls.write_all(b"GET /r HTTP/1.1\r\nHost: api.test\r\nX-Mode: reset\r\n\r\n")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    let read = tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut out)).await;
    assert!(matches!(read, Ok(Err(_))), "{read:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn tls_22_an_open_exchange_at_the_end_of_the_grace_is_written_as_shutdown() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    tls.write_all(b"GET /h HTTP/1.1\r\nHost: api.test\r\nX-Mode: hang\r\n\r\n")
        .await
        .unwrap();
    eventually("request reached the destination", || {
        !dest.sni.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    sidecar.shared.shutdown();
    let closed = sidecar.shared.drain(Duration::from_millis(200)).await;
    assert_eq!(closed, 1);
    eventually("shutdown line", || {
        logs.access()
            .iter()
            .any(|l| l["result"] == "shutdown" && l["tls"] == true)
    })
    .await;
}

#[tokio::test]
async fn dsc_21_intercepted_outbound_access_line_redacts_path_secrets() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";
    let request =
        format!("GET /reset/{jwt} HTTP/1.1\r\nHost: api.test:443\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(exchange(&mut tls, request.as_bytes()).await, RESPONSE);
    eventually("the access line", || !logs.access().is_empty()).await;
    let lines = logs.access();
    assert_eq!(lines[0]["tls"], true);
    assert_eq!(lines[0]["path"], "/reset/<redacted>");
    assert!(!logs.text().contains(jwt), "{}", logs.text());
}
