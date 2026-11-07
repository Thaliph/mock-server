//! MCK-13: TLS in mock (spec §5 "Mock mode", §8), in process. The destination is a listener that
//! counts the connections it accepts and never speaks; the client trusts the deployment root
//! only. Every wait has a deadline.

mod common;

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::mock_rig::{AUTHORITY, Rig, TlsSetup, rig_full, users_set};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_sidecar::tls::ca::{CaMetrics, CaState};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::{TlsConnector, client::TlsStream};
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

const TIMEOUT: Duration = Duration::from_secs(5);

fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

/// The deployment root and a `CaState` holding an intermediate under it.
fn ca_state() -> (Arc<CaState>, CertificateDer<'static>) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "mimic root".to_string());
    let root_key = KeyPair::generate().unwrap();
    let root_der = params.self_signed(&root_key).unwrap().der().clone();
    let root = Issuer::new(params, root_key);
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

async fn mock_rig(exclude: &'static str, ca: Option<Arc<CaState>>) -> Rig {
    rig_full(
        users_set(5),
        100,
        1024,
        None,
        Some(TlsSetup { exclude, ca }),
    )
    .await
}

fn connector(root: &CertificateDer<'static>, alpn: &[&[u8]]) -> TlsConnector {
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

async fn handshake(
    rig: &Rig,
    root: &CertificateDer<'static>,
    name: ServerName<'static>,
    alpn: &[&[u8]],
) -> io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(rig.addr).await?;
    tokio::time::timeout(TIMEOUT, connector(root, alpn).connect(name, tcp))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake"))?
}

fn name(host: &str) -> ServerName<'static> {
    ServerName::try_from(host.to_string()).unwrap()
}

/// The subject alternative names of the leaf the client was given.
fn leaf_names(tls: &TlsStream<TcpStream>) -> Vec<String> {
    let chain = tls.get_ref().1.peer_certificates().expect("a chain");
    let (_, cert) = X509Certificate::from_der(chain[0].as_ref()).unwrap();
    let (_, issuer) = X509Certificate::from_der(chain[1].as_ref()).unwrap();
    assert!(
        issuer.subject().to_string().contains("mimic intermediate"),
        "issued by {}",
        issuer.subject()
    );
    cert.extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => Some(san),
            _ => None,
        })
        .flat_map(|san| san.general_names.iter())
        .map(|n| match n {
            GeneralName::DNSName(d) => (*d).to_string(),
            GeneralName::IPAddress(ip) => format!("ip:{ip:?}"),
            other => format!("{other:?}"),
        })
        .collect()
}

async fn http1_get(tls: &mut TlsStream<TcpStream>) -> String {
    let request = format!("GET /users/3 HTTP/1.1\r\nHost: {AUTHORITY}\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(TIMEOUT, tls.read(&mut buf))
            .await
            .expect("answered in time")
            .unwrap();
        assert!(n > 0, "closed before the answer");
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&got).into_owned()
}

#[tokio::test]
async fn mck_13_h2_is_negotiated_and_the_mocked_answer_comes_with_a_leaf_for_the_sni() {
    let (ca, root) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let tls = handshake(&rig, &root, name("api.test"), &[b"h2", b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    assert_eq!(leaf_names(&tls), ["api.test"]);
    let (mut send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(conn);
    let request = Request::builder()
        .uri(format!("https://{AUTHORITY}/users/3"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = tokio::time::timeout(TIMEOUT, send.send_request(request))
        .await
        .expect("answered in time")
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("x-mimic-match"));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["name"], "user");

    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.counter("mimic_mock_connections_total{decision=\"mocked\"}"),
        1
    );
    assert_eq!(
        rig.counter("mimic_tls_connections_total{interception=\"intercepted\"}"),
        1
    );
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["protocol"], "h2");
    assert_eq!(access[0]["tls"], true);
    assert_eq!(access[0]["mock"].as_str().is_some(), true);
}

#[tokio::test]
async fn mck_13_http_1_1_only_gets_http_1_1() {
    let (ca, root) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let mut tls = handshake(&rig, &root, name("api.test"), &[b"http/1.1"])
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    let head = http1_get(&mut tls).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("x-mimic-match"),
        "{head}"
    );
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["protocol"], "http1");
    assert_eq!(access[0]["tls"], true);
}

#[tokio::test]
async fn mck_13_a_client_without_alpn_is_served_by_what_it_speaks() {
    let (ca, root) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let mut tls = handshake(&rig, &root, name("api.test"), &[]).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), None);
    let head = http1_get(&mut tls).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[tokio::test]
async fn mck_13_a_hello_without_sni_to_an_ip_gets_a_leaf_with_that_ip_san() {
    let (ca, root) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let ip = ServerName::IpAddress("127.0.0.1".parse::<std::net::IpAddr>().unwrap().into());
    let mut tls = handshake(&rig, &root, ip, &[b"http/1.1"]).await.unwrap();
    let names = leaf_names(&tls);
    assert!(
        names.iter().any(|n| n.contains("127, 0, 0, 1")),
        "{names:?}"
    );
    let head = http1_get(&mut tls).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

/// A TLS ClientHello offering TLS 1.1 at most, with this SNI.
fn tls11_hello(sni: &str) -> Vec<u8> {
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

/// The connection ended without a byte of an answer to a request.
async fn closed_without_answer(tcp: &mut TcpStream) {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, tcp.read_to_end(&mut out))
        .await
        .expect("closed in time");
    // A TLS alert may precede the close; an HTTP answer must not.
    assert!(
        !String::from_utf8_lossy(&out).contains("HTTP/1.1"),
        "answered: {}",
        String::from_utf8_lossy(&out)
    );
}

async fn assert_closed_by_mock(rig: &Rig, interception: Option<&str>, tls_series: Option<&str>) {
    let access = rig.logs.access_exactly(1).await;
    assert_eq!(access[0]["message"], "connection");
    assert_eq!(access[0]["result"], "mock_closed", "{access:?}");
    match interception {
        Some(why) => assert_eq!(access[0]["interception"], why),
        None => assert!(access[0]["interception"].is_null()),
    }
    assert_eq!(
        rig.counter("mimic_mock_connections_total{decision=\"closed\"}"),
        1
    );
    if let Some(series) = tls_series {
        assert_eq!(rig.counter(series), 1, "{series}");
    }
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mck_13_an_excluded_sni_is_closed_with_nothing_at_the_destination() {
    let (ca, root) = ca_state();
    let rig = mock_rig("api.test", Some(ca)).await;
    let failed = handshake(&rig, &root, name("api.test"), &[b"http/1.1"]).await;
    assert!(failed.is_err());
    assert_closed_by_mock(
        &rig,
        Some("excluded"),
        Some("mimic_tls_connections_total{interception=\"excluded\"}"),
    )
    .await;
    let access = rig.logs.access();
    assert_eq!(access[0]["sni"], "api.test");
    assert_eq!(access[0]["protocol"], "tls");
}

#[tokio::test]
async fn mck_13_a_sidecar_without_a_ca_closes_the_connection() {
    let (_, root) = ca_state();
    let rig = mock_rig("", None).await;
    let failed = handshake(&rig, &root, name("api.test"), &[b"http/1.1"]).await;
    assert!(failed.is_err());
    assert_closed_by_mock(
        &rig,
        Some("no_ca"),
        Some("mimic_tls_connections_total{interception=\"no_ca\"}"),
    )
    .await;
}

#[tokio::test]
async fn mck_13_a_tls_1_1_hello_is_closed_with_nothing_at_the_destination() {
    let (ca, _) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(&tls11_hello("api.test")).await.unwrap();
    closed_without_answer(&mut tcp).await;
    assert_closed_by_mock(
        &rig,
        Some("unsupported"),
        Some("mimic_tls_connections_total{interception=\"unsupported\"}"),
    )
    .await;
}

#[tokio::test]
async fn mck_13_a_decrypted_stream_that_is_not_http_is_closed() {
    let (ca, root) = ca_state();
    let rig = mock_rig("", Some(ca)).await;
    let mut tls = handshake(&rig, &root, name("api.test"), &[]).await.unwrap();
    // A PostgreSQL `SSLRequest`.
    tls.write_all(&[0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(TIMEOUT, tls.read_to_end(&mut out))
        .await
        .expect("closed in time");
    assert!(out.is_empty());
    assert_closed_by_mock(&rig, None, None).await;
}
