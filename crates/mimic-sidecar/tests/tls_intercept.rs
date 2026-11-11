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
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
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

// What the token endpoint returns and what the client sends it (EGR-17). None may leave the
// sidecar.
const ACCESS_TOKEN: &str = "CANARY-ACCESSTOKEN-92e";
const SET_COOKIE: &str = "CANARY-SETCOOKIE-92e";
const CLIENT_SECRET: &str = "CANARY-CLIENTSECRET-92e";
const BEARER: &str = "CANARY-BEARER-92e";
const API_KEY: &str = "CANARY-APIKEY-92e";
const QUERY_KEY: &str = "CANARY-QKEY-92e";
const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.CANARYJWTSIGNATURE92e";
const GOOGLE: &str = "AIzaCANARY0123456789abcdefghijklmnopqrs";

fn token_reply() -> Vec<u8> {
    let body = format!("{{\"access_token\":\"{ACCESS_TOKEN}\",\"token_type\":\"bearer\"}}");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: sid={SET_COOKIE}; HttpOnly\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

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

async fn destination_with(
    alpn: &[&[u8]],
    trusted: bool,
    client_ca: Option<&CertificateDer<'static>>,
) -> Destination {
    destination_full(alpn, trusted, client_ca, None).await
}

/// `trusted`: its CA is in the image trust store; `client_ca`: client certificates signed by
/// it are requested (not required); `banner`: written first, before the client says anything.
async fn destination_full(
    alpn: &[&[u8]],
    trusted: bool,
    client_ca: Option<&CertificateDer<'static>>,
    banner: Option<&'static [u8]>,
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
                if let Some(banner) = banner {
                    let _ = tls.write_all(banner).await;
                }
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
                if got.windows(13).any(|w| w == b"X-Mode: token") {
                    let _ = tls.write_all(&token_reply()).await;
                    let mut rest = Vec::new();
                    let _ = tls.read_to_end(&mut rest).await;
                    got.extend_from_slice(&rest);
                    read.lock().unwrap().push(got.clone());
                    let _ = tls.shutdown().await;
                    return;
                }
                if got.windows(12).any(|w| w == b"X-Mode: slow") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                if got.windows(12).any(|w| w == b"X-Mode: gzip") {
                    use std::io::Write;
                    let mut gz =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    gz.write_all(br#"{"gz":true,"items":[1,2]}"#).unwrap();
                    let body = gz.finish().unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(&body).await;
                    let _ = tls.shutdown().await;
                    return;
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
    sidecar_to(dest, dest.addr, ca, exclusions, direction, None).await
}

/// The sidecar sending to `to` (the destination, or something in front of it), recording
/// into `discovery`.
async fn sidecar_to(
    dest: &Destination,
    to: SocketAddr,
    ca: Option<Arc<CaState>>,
    exclusions: &str,
    direction: Direction,
    discovery: Option<&Discovery>,
) -> Sidecar {
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
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
    let original: OriginalDst = Arc::new(move |_| Ok(to));
    let mut shared = Shared::new(vec![addr], tunnel, original)
        .with_proxy_metrics(proxy)
        .with_tls(interception);
    if let Some(discovery) = discovery {
        discovery.register(&mut metrics);
        shared = shared.with_discovery(discovery.tap());
    }
    let shared = Arc::new(shared);
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
    eventually("warned", || warns(&logs) == 1).await;
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
    // Remembered: the second connection adds no WARN.
    assert_eq!(warns(&logs), 1, "{}", logs.text());
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
    // The log worker writes asynchronously: wait for the line, then check that it is the only one.
    eventually("warned", || warns(&logs) == 1).await;
    logs.settled().await;
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
    assert_eq!(lines[0]["path"], "/reset/mimic-jwt");
    assert!(!logs.text().contains(jwt), "{}", logs.text());
}

#[tokio::test]
async fn tls_27_a_client_that_resets_during_the_handshake_is_not_remembered() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let addr = sidecar.addr;
    // Sends its ClientHello, reads the sidecar's first flight, then resets.
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let mut conn =
            tokio_rustls::rustls::ClientConnection::new(Arc::new(config), api()).unwrap();
        let mut tcp = std::net::TcpStream::connect(addr).unwrap();
        let mut hello = Vec::new();
        conn.write_tls(&mut hello).unwrap();
        tcp.write_all(&hello).unwrap();
        let mut flight = [0u8; 4096];
        assert!(tcp.read(&mut flight).unwrap() > 0);
        socket2::SockRef::from(&tcp)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(warns(&logs), 0, "{}", logs.text());
    assert!(
        sidecar
            .metrics
            .encode()
            .contains("mimic_tls_passthrough_destinations 0")
    );
    // Still intercepted: the next client trusting the mimic root gets its answer.
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
}

#[tokio::test]
async fn tls_27_a_server_first_protocol_over_an_intercepted_connection_gets_its_banner() {
    let dest = destination_full(&[], true, None, Some(b"220 hello\r\n")).await;
    let (ca, root) = ca_state();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let mut banner = [0u8; 11];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut banner))
        .await
        .expect("the banner reaches a client that is waiting for it")
        .unwrap();
    assert_eq!(&banner, b"220 hello\r\n");
}

#[tokio::test]
async fn tls_27_unreadable_and_unparsable_root_files_are_logged() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    // A directory where a file is expected, and a block that is not a certificate.
    std::fs::create_dir_all(dest.trust.path().join("trust/1/image-ca.pem")).unwrap();
    std::fs::create_dir_all(dest.trust.path().join("trust/2")).unwrap();
    std::fs::write(
        dest.trust.path().join("trust/2/image-ca.pem"),
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let sidecar = sidecar(&dest, Some(ca), "").await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    let text = logs.text();
    assert_eq!(text.matches("tls roots skipped").count(), 2, "{text}");
    assert!(text.contains("trust/1/image-ca.pem") && text.contains("trust/2/image-ca.pem"));
}

// ---------------------------------------------------------------------------------------
// EGR-10 and EGR-12: the decrypted exchanges are recorded as outbound discovery.

fn discovery() -> Discovery {
    Discovery::start(&DiscoveryConfig::default(), Some("pod-a".into())).unwrap()
}

fn scope(discovery: &Discovery, authority: &str) -> serde_json::Value {
    let json: serde_json::Value =
        serde_json::from_str(&discovery.report().to_json().unwrap()).unwrap();
    json["scopes"]["outbound"][authority].clone()
}

async fn recorded(discovery: &Discovery, n: u64) {
    eventually("the recorder holds the exchanges", || {
        discovery.report().exchanges() >= n
    })
    .await;
}

#[tokio::test]
async fn egr_10_intercepted_http1_is_recorded_decrypted_under_the_sni_authority() {
    let _logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    // A `Host` that differs from the SNI keys the endpoint by `Host`.
    let other = b"GET /b HTTP/1.1\r\nHost: other.example:443\r\nContent-Length: 0\r\n\r\n";
    let mut second = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert_eq!(exchange(&mut second, other).await, RESPONSE);
    recorded(&discovery, 2).await;
    let report = discovery.report();
    let found: Vec<_> = report
        .endpoints()
        .iter()
        .map(|e| (e.authority.unwrap().to_string(), e.template.clone()))
        .collect();
    assert_eq!(
        found,
        [
            ("api.test:443".to_string(), "/a".to_string()),
            ("other.example:443".to_string(), "/b".to_string())
        ]
    );
    let dst = format!("127.0.0.1:{}", dest.addr.port());
    for authority in ["api.test:443", "other.example:443"] {
        let origin = &scope(&discovery, authority)["origin"];
        assert_eq!(origin["schemes"], serde_json::json!({"https": 1}));
        assert_eq!(origin["sni"], serde_json::json!(["api.test"]));
        assert_eq!(origin["dst"], serde_json::json!([dst]));
    }
    // The decrypted head and its redaction: the canary of the request is not kept.
    let json = report.to_json().unwrap();
    assert!(json.contains("x-canary"));
    assert!(!json.contains("canary-header-91"));
    assert!(!json.contains("canary-cookie-91"));
}

#[tokio::test]
async fn egr_10_a_gzip_response_over_tls_is_decoded_and_inferred() {
    let _logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    tls.write_all(
        b"GET /gz HTTP/1.1\r\nHost: api.test\r\nX-Mode: gzip\r\nContent-Length: 0\r\n\r\n",
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut out)).await;
    assert!(out.starts_with(b"HTTP/1.1 200"));
    recorded(&discovery, 1).await;
    let json = discovery.report().to_json().unwrap();
    assert!(json.contains("\"gz\""), "{json}");
    assert!(json.contains("items"));
}

#[tokio::test]
async fn egr_10_intercepted_h2_is_recorded_decrypted() {
    let _logs = common::capture_logs();
    let dest = destination(&[b"h2", b"http/1.1"]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let tls = connect(&sidecar, &root, api(), &[b"h2"]).await.unwrap();
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
        let _ = resp.into_body().collect().await;
    }
    recorded(&discovery, 2).await;
    let templates: Vec<_> = discovery
        .report()
        .endpoints()
        .iter()
        .map(|e| (e.authority.unwrap().to_string(), e.template.clone()))
        .collect();
    assert_eq!(
        templates,
        [
            ("api.test:443".to_string(), "/one".to_string()),
            ("api.test:443".to_string(), "/two".to_string())
        ]
    );
    let origin = &scope(&discovery, "api.test:443")["origin"];
    assert_eq!(origin["schemes"], serde_json::json!({"https": 2}));
    assert_eq!(origin["sni"], serde_json::json!(["api.test"]));
}

/// The bucket the single observation of the only endpoint is in.
fn bucket(discovery: &Discovery) -> u8 {
    let report = discovery.report();
    let endpoints = report.endpoints();
    *endpoints[0].stats.latency.buckets().keys().next().unwrap()
}

#[tokio::test]
async fn egr_12_latency_is_the_time_to_the_response_head_as_the_access_line_says() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    let slow = b"GET /slow HTTP/1.1\r\nHost: api.test\r\nX-Mode: slow\r\nContent-Length: 0\r\n\r\n";
    assert_eq!(exchange(&mut tls, slow).await, RESPONSE);
    recorded(&discovery, 1).await;
    let lines = logs.access_exactly(1).await;
    let head_ms = lines[0]["head_ms"].as_u64().unwrap();
    assert!(head_ms >= 300, "{head_ms}");
    assert!(bucket(&discovery) >= mimic_core::discovery::histogram::bucket_index(300));
    assert_eq!(
        bucket(&discovery),
        mimic_core::discovery::histogram::bucket_index(head_ms)
    );
}

/// A TCP relay that holds each connection for `delay` before connecting to `to`: the TLS
/// handshake with the destination starts late.
async fn delayed(to: SocketAddr, delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Ok(mut outbound) = TcpStream::connect(to).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn egr_12_the_handshake_with_the_destination_is_not_in_the_latency() {
    let _logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let front = delayed(dest.addr, Duration::from_millis(500)).await;
    let sidecar = sidecar_to(
        &dest,
        front,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let started = std::time::Instant::now();
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(exchange(&mut tls, REQUEST).await, RESPONSE);
    recorded(&discovery, 1).await;
    assert!(bucket(&discovery) < mimic_core::discovery::histogram::bucket_index(500));
}

// ---------------------------------------------------------------------------------------
// EGR-11: the TLS connections that are passed through are counted as tunnels.

/// Waits for one `kind` tunnel under `authority`, and checks the metric, the lack of any
/// endpoint and that the count is exactly one.
async fn tunnelled(sidecar: &Sidecar, discovery: &Discovery, authority: &str, kind: &str) {
    eventually("the tunnel is recorded", || {
        scope(discovery, authority)["tunnels"][kind] == 1
    })
    .await;
    common::counted(
        &sidecar.metrics,
        &format!("mimic_discovery_tunnels_total{{kind=\"{kind}\",result=\"recorded\"}} 1"),
    )
    .await;
    assert_eq!(
        scope(discovery, authority)["tunnels"],
        serde_json::json!({ kind: 1 })
    );
    assert!(discovery.report().endpoints().is_empty());
    assert_eq!(discovery.report().sample_bytes(), 0);
}

#[tokio::test]
async fn egr_11_an_untrusted_destination_is_a_tunnel_of_its_sni_and_port() {
    let _logs = common::capture_logs();
    let dest = destination_with(&[], false, None).await;
    let (ca, _root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let tls = own_view(&sidecar, &dest).await;
    drop(tls);
    let authority = format!("api.test:{}", dest.addr.port());
    tunnelled(&sidecar, &discovery, &authority, "tls_upstream_untrusted").await;
}

#[tokio::test]
async fn egr_11_an_mtls_destination_is_a_tunnel_and_the_client_certificate_still_reaches_it() {
    let _logs = common::capture_logs();
    let (client_ca, client_ca_der) = authority("client ca");
    let dest = destination_with(&[], true, Some(&client_ca_der)).await;
    let (ca, _root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
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
    assert!(dest.client_certs.lock().unwrap().contains(&true));
    let authority = format!("api.test:{}", dest.addr.port());
    tunnelled(
        &sidecar,
        &discovery,
        &authority,
        "tls_client_cert_requested",
    )
    .await;
}

#[tokio::test]
async fn egr_11_excluded_unsupported_and_no_ca_connections_are_tunnels() {
    let _logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let port = dest.addr.port();
    let out = Direction::Outbound;

    let excluded = discovery();
    let a = sidecar_to(
        &dest,
        dest.addr,
        Some(ca.clone()),
        "*.excluded.test",
        out,
        Some(&excluded),
    )
    .await;
    let name = ServerName::try_from("a.excluded.test").unwrap();
    let _ = connect(&a, &root, name, &[]).await;
    let key = format!("a.excluded.test:{port}");
    tunnelled(&a, &excluded, &key, "tls_excluded").await;

    let unsupported = discovery();
    let b = sidecar_to(&dest, dest.addr, Some(ca), "", out, Some(&unsupported)).await;
    let _ = connect(&b, &root, api(), &[b"imap"]).await;
    let key = format!("api.test:{port}");
    tunnelled(&b, &unsupported, &key, "tls_unsupported").await;

    let no_ca = discovery();
    let c = sidecar_to(&dest, dest.addr, None, "", out, Some(&no_ca)).await;
    let _ = connect(&c, &root, api(), &[]).await;
    tunnelled(&c, &no_ca, &key, "tls_no_ca").await;
}

#[tokio::test]
async fn egr_11_a_client_that_rejects_the_mimic_certificate_is_a_tunnel() {
    let _logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, _root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    assert!(connect(&sidecar, &dest.ca_der, api(), &[]).await.is_err());
    let authority = format!("api.test:{}", dest.addr.port());
    tunnelled(&sidecar, &discovery, &authority, "tls_client_rejected").await;
}

#[tokio::test]
async fn egr_17_an_intercepted_token_exchange_leaves_no_canary_and_no_byte_changes() {
    let logs = common::capture_logs();
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let discovery = discovery();
    let sidecar = sidecar_to(
        &dest,
        dest.addr,
        Some(ca),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let form = format!("grant_type=client_credentials&client_id=app&client_secret={CLIENT_SECRET}");
    let request = format!(
        "POST /oauth/token/{JWT}?key={QUERY_KEY}&maps={GOOGLE} HTTP/1.1\r\nHost: api.test:443\r\n\
         X-Mode: token\r\nAuthorization: Bearer {BEARER}\r\nX-Api-Key: {API_KEY}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    );
    let mut tls = connect(&sidecar, &root, api(), &[]).await.unwrap();
    tls.write_all(request.as_bytes()).await.unwrap();
    // The response reaches the client unchanged.
    let expected = token_reply();
    let mut got = vec![0u8; expected.len()];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut got))
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(got, expected);
    recorded(&discovery, 1).await;
    // The request reaches the destination unchanged.
    tls.shutdown().await.unwrap();
    eventually("the destination read the request", || {
        !dest.requests.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(dest.requests.lock().unwrap()[0], request.as_bytes());
    let report = discovery.report().to_json().unwrap();
    assert!(report.contains("access_token") && report.contains("client_secret"));
    logs.settled().await;
    let texts = [
        ("report", report),
        ("metrics", sidecar.metrics.encode()),
        ("logs", logs.text()),
    ];
    let canaries = [
        ACCESS_TOKEN,
        SET_COOKIE,
        CLIENT_SECRET,
        BEARER,
        API_KEY,
        QUERY_KEY,
        JWT,
        "CANARYJWTSIGNATURE",
        GOOGLE,
    ];
    for (what, text) in &texts {
        for (i, canary) in canaries.iter().enumerate() {
            let leaked = text.contains(canary);
            assert!(!leaked, "canary {i} in {what}");
        }
    }
}

// ---------------------------------------------------------------------------------------
// EGR-14: shedding under a heavy outbound load.

const CANNED: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
const BIG: usize = 200 * 1024;

/// An app that checks each 200 KiB request body (all `z`) and answers `CANNED`, or a `500`.
async fn checking_app() -> SocketAddr {
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut got = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                let end = loop {
                    if let Some(p) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                };
                while got.len() < end + BIG {
                    match s.read(&mut buf).await {
                        Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let intact = got[end..].iter().all(|&b| b == b'z');
                let reply: &[u8] = if intact {
                    CANNED
                } else {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                };
                let _ = s.write_all(reply).await;
            });
        }
    });
    addr
}

fn big_request(host: &str) -> Vec<u8> {
    let mut r = format!(
        "POST /upload HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {BIG}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    r.extend(vec![b'z'; BIG]);
    r
}

fn direction_sum(text: &str, direction: &str, result: Option<&str>) -> u64 {
    text.lines()
        .filter(|l| l.starts_with("mimic_discovery_exchanges_total{"))
        .filter(|l| l.contains(&format!("direction=\"{direction}\"")))
        .filter(|l| result.is_none_or(|r| l.contains(&format!("result=\"{r}\""))))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egr_14_outbound_shedding_keeps_inbound_recorded_and_traffic_intact() {
    let _logs = common::capture_logs();
    mimic_sidecar::listener::raise_fd_limit();
    let app = checking_app().await;
    let dest = destination(&[]).await;
    let (ca, root) = ca_state();
    let config = DiscoveryConfig {
        queue: 1,
        capture_budget: 1024 * 1024,
        ..DiscoveryConfig::default()
    };
    let discovery = Discovery::start(&config, Some("pod-a".into())).unwrap();
    let clear = sidecar_to(
        &dest,
        app,
        Some(ca.clone()),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let tls_out = sidecar_to(
        &dest,
        dest.addr,
        Some(ca.clone()),
        "",
        Direction::Outbound,
        Some(&discovery),
    )
    .await;
    let inbound = sidecar_to(
        &dest,
        app,
        Some(ca),
        "",
        Direction::Inbound,
        Some(&discovery),
    )
    .await;
    let request = Arc::new(big_request("api.example.test:8080"));
    let inbound_request = Arc::new(big_request("svc.ns:8080"));
    let mut tasks = Vec::new();
    for i in 0..2000 {
        if i % 10 == 9 {
            // 200 intercepted exchanges.
            let (addr, root) = (tls_out.addr, root.clone());
            let request = request.clone();
            tasks.push(tokio::spawn(async move {
                let tcp = TcpStream::connect(addr).await.unwrap();
                let connector = client_config(&root, &[]);
                let mut tls =
                    tokio::time::timeout(Duration::from_secs(60), connector.connect(api(), tcp))
                        .await
                        .expect("handshake in time")
                        .unwrap();
                tls.write_all(&request).await.unwrap();
                let mut got = vec![0u8; RESPONSE.len()];
                tokio::time::timeout(Duration::from_secs(60), tls.read_exact(&mut got))
                    .await
                    .expect("answered in time")
                    .unwrap();
                got == RESPONSE
            }));
        } else {
            tasks.push(tokio::spawn(clear_call(clear.addr, request.clone())));
        }
        if i % 10 == 0 {
            tasks.push(tokio::spawn(clear_call(
                inbound.addr,
                inbound_request.clone(),
            )));
        }
    }
    for task in tasks {
        assert!(
            task.await.unwrap(),
            "an answer differs from what the destination sent"
        );
    }
    for _ in 0..400 {
        let text = clear.metrics.encode();
        if direction_sum(&text, "outbound", None) == 2000
            && direction_sum(&text, "inbound", None) == 200
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = clear.metrics.encode();
    assert_eq!(direction_sum(&text, "outbound", None), 2000, "{text}");
    assert_eq!(direction_sum(&text, "inbound", None), 200, "{text}");
    assert!(
        direction_sum(&text, "outbound", Some("dropped")) > 0,
        "{text}"
    );
    assert!(
        direction_sum(&text, "inbound", Some("recorded")) >= 1,
        "{text}"
    );
    let budget: u64 = text
        .lines()
        .filter(|l| {
            l.starts_with("mimic_discovery_bodies_total{") && l.contains("result=\"budget\"")
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    assert!(budget > 0, "{text}");
}

async fn clear_call(addr: SocketAddr, request: Arc<Vec<u8>>) -> bool {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&request).await.unwrap();
    let mut got = vec![0u8; CANNED.len()];
    tokio::time::timeout(Duration::from_secs(60), c.read_exact(&mut got))
        .await
        .expect("answered in time")
        .unwrap();
    got == CANNED
}
