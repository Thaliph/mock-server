//! MSH-72 to MSH-74: TLS that §5 intercepts is re-originated over TLS to the pods of an
//! in-scope Service (spec §7.9 "Intercepted TLS"): SNI the app's, the certificate verified
//! against the Service name and never the pod IP, no cleartext, no silent downgrade.

mod common;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::capture_logs;
use common::mesh_pods::view_on;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
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

const TIMEOUT: Duration = Duration::from_secs(5);
const NAME: &str = "cart.shop.svc";

type Authority = Issuer<'static, KeyPair>;

fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
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
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
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

/// A pod serving TLS: HTTP/1 (or h2 when it agreed `h2`) answering its name; `Upgrade`
/// requests get a `101` and an echo, a connection starting with a `NUL` byte is echoed.
struct TlsPod {
    addr: SocketAddr,
    name: String,
    /// Complete requests read.
    requests: Arc<AtomicUsize>,
    /// SNI of each completed handshake.
    sni: Arc<Mutex<Vec<Option<String>>>>,
    /// TCP connections accepted.
    accepts: Arc<AtomicUsize>,
}

async fn tls_pod(name: &str, ca: &Authority, sans: Vec<SanType>, alpn: &[&[u8]]) -> TlsPod {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = sans;
    let cert = params.signed_by(&key, ca).unwrap();
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
    let pod = TlsPod {
        addr: listener.local_addr().unwrap(),
        name: name.to_string(),
        requests: Arc::default(),
        sni: Arc::default(),
        accepts: Arc::default(),
    };
    let (requests, sni, accepts, name) = (
        pod.requests.clone(),
        pod.sni.clone(),
        pod.accepts.clone(),
        pod.name.clone(),
    );
    let own = pod.addr.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            accepts.fetch_add(1, Ordering::SeqCst);
            let (acceptor, requests, sni, name, own) = (
                acceptor.clone(),
                requests.clone(),
                sni.clone(),
                name.clone(),
                own.clone(),
            );
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                sni.lock()
                    .unwrap()
                    .push(tls.get_ref().1.server_name().map(str::to_string));
                if tls.get_ref().1.alpn_protocol() == Some(&b"h2"[..]) {
                    let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                        requests.fetch_add(1, Ordering::SeqCst);
                        let body = name.clone();
                        async move { Ok::<_, io::Error>(Response::new(Full::new(Bytes::from(body)))) }
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                    return;
                }
                let mut pending = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let Ok(n) = tls.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    if pending.is_empty() && buf[0] == 0 {
                        let _ = tls.write_all(&buf[..n]).await;
                        echo(&mut tls).await;
                        return;
                    }
                    pending.extend_from_slice(&buf[..n]);
                    while let Some(at) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head: Vec<u8> = pending.drain(..at + 4).collect();
                        requests.fetch_add(1, Ordering::SeqCst);
                        if String::from_utf8_lossy(&head)
                            .to_ascii_lowercase()
                            .contains("upgrade: ")
                        {
                            let _ = tls
                                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: echo\r\n\r\n")
                                .await;
                            echo(&mut tls).await;
                            return;
                        }
                        // A pod named `refuser` refuses as an overloaded sidecar does.
                        let answer = if name == "apprefuser" {
                            format!(
                                "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: connect-failed\r\nx-mimic-endpoint: {own}\r\nx-mimic-origin: inbound; pod=x\r\ncontent-length: 0\r\n\r\n"
                            )
                        } else if name == "refuser" {
                            format!(
                                "HTTP/1.1 503 Service Unavailable\r\nx-mimic-error: overloaded\r\nx-mimic-endpoint: {own}\r\ncontent-length: 0\r\n\r\n"
                            )
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{name}",
                                name.len()
                            )
                        };
                        if tls.write_all(answer.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    pod
}

async fn echo(tls: &mut tokio_rustls::server::TlsStream<TcpStream>) {
    let mut buf = [0u8; 1024];
    while let Ok(n) = tls.read(&mut buf).await {
        if n == 0 || tls.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

fn names(extra: &[&str]) -> Vec<SanType> {
    let mut sans: Vec<SanType> = extra
        .iter()
        .map(|n| SanType::DnsName((*n).try_into().unwrap()))
        .collect();
    sans.sort_by_key(|s| format!("{s:?}"));
    sans
}

struct Rig {
    addr: SocketAddr,
    root: CertificateDer<'static>,
    metrics: Arc<Metrics>,
    _trust: tempfile::TempDir,
}

/// The sidecar's outbound listener: the Service `shop/cart` with `pods` and the ClusterIP
/// `cluster`, §5 interception with `trusted` in the image trust store.
async fn rig(trusted: &CertificateDer<'static>, cluster: SocketAddr, pods: &[SocketAddr]) -> Rig {
    let trust = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(trust.path().join("trust/0")).unwrap();
    std::fs::write(trust.path().join("trust/0/image-ca.pem"), pem(trusted)).unwrap();
    let (ca, root) = ca_state();
    let listener = mimic_sidecar::listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut metrics = Metrics::new("sidecar", "test");
    let tunnel = TunnelMetrics::register(&mut metrics);
    let proxy = ProxyMetrics::register(&mut metrics, 50);
    let tls = TlsMetrics::register(&mut metrics);
    let interception = Interception::new(
        Exclusions::parse("").unwrap(),
        Some(ca),
        Vec::<PathBuf>::new(),
        trust.path().to_path_buf(),
        tls,
    );
    let config = MeshConfig {
        enabled: true,
        seed: Some(11),
        ..MeshConfig::default()
    };
    let state = MeshState::new(&config);
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state.register(&mut metrics);
    let original: OriginalDst = Arc::new(move |_| Ok(cluster));
    let shared = Arc::new(
        Shared::new(vec![addr], tunnel, original)
            .with_proxy_metrics(proxy)
            .with_tls(interception)
            .with_mesh(state),
    );
    spawn_listener(listener, Direction::Outbound, shared);
    Rig {
        addr,
        root,
        metrics: Arc::new(metrics),
        _trust: trust,
    }
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

async fn client(
    rig: &Rig,
    name: ServerName<'static>,
    alpn: &[&[u8]],
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    tokio::time::timeout(TIMEOUT, connector(&rig.root, alpn).connect(name, tcp))
        .await
        .expect("handshake in time")
        .expect("handshake")
}

fn service() -> ServerName<'static> {
    ServerName::try_from(NAME).unwrap()
}

/// One HTTP/1 request and its answer: the status line and the body.
async fn get(tls: &mut tokio_rustls::client::TlsStream<TcpStream>) -> (String, String) {
    let request = format!("GET /a HTTP/1.1\r\nHost: {NAME}\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let head_end = got.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(at) = head_end {
            let head = String::from_utf8_lossy(&got[..at]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")?
                        .parse()
                        .ok()
                })
                .unwrap_or(0);
            if got.len() >= at + 4 + len {
                let body = String::from_utf8_lossy(&got[at + 4..at + 4 + len]).to_string();
                return (head.lines().next().unwrap().to_string(), body);
            }
        }
        let n = tokio::time::timeout(TIMEOUT, tls.read(&mut buf))
            .await
            .expect("an answer in time")
            .expect("a read");
        assert!(n > 0, "closed before the answer: {got:?}");
        got.extend_from_slice(&buf[..n]);
    }
}

fn counter(metrics: &str, prefix: &str) -> f64 {
    metrics
        .lines()
        .filter(|l| l.starts_with(prefix))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn cluster() -> SocketAddr {
    "127.0.0.1:80".parse().unwrap()
}

#[tokio::test]
async fn msh_72_intercepted_http1_is_re_originated_to_every_pod_with_the_sni_verified() {
    let logs = capture_logs();
    let (pod_ca, pod_root) = authority("pods");
    let sans = || names(&[NAME]);
    let a = tls_pod("a", &pod_ca, sans(), &[]).await;
    let b = tls_pod("b", &pod_ca, sans(), &[]).await;
    let c = tls_pod("c", &pod_ca, sans(), &[]).await;
    let rig = rig(&pod_root, cluster(), &[a.addr, b.addr, c.addr]).await;
    let mut answered = std::collections::BTreeSet::new();
    for sni in [true, false] {
        for _ in 0..30 {
            let name = if sni {
                service()
            } else {
                ServerName::try_from("127.0.0.1").unwrap()
            };
            let mut tls = client(&rig, name, &[]).await;
            let (status, body) = get(&mut tls).await;
            assert_eq!(status, "HTTP/1.1 200 OK");
            answered.insert(body);
        }
    }
    assert_eq!(answered.len(), 3, "every pod answered: {answered:?}");
    for pod in [&a, &b, &c] {
        assert!(pod.requests.load(Ordering::SeqCst) >= 5, "pod {}", pod.name);
        let seen = pod.sni.lock().unwrap().clone();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|s| s.as_deref() == Some(NAME)),
            "the pods saw SNI {seen:?}"
        );
    }
    logs.settled().await;
    let lines = logs.access();
    let pods: Vec<String> = [&a, &b, &c].iter().map(|p| p.addr.to_string()).collect();
    assert_eq!(lines.len(), 60, "{}", logs.text());
    for line in &lines {
        assert_eq!(line["tls"], true);
        assert_eq!(line["backend"], "shop/cart:80");
        assert_eq!(line["dst"], "127.0.0.1:80");
        assert!(pods.contains(&line["upstream"].as_str().unwrap().to_string()));
    }
    assert_eq!(
        counter(&rig.metrics.encode(), "mimic_mesh_fallbacks_total"),
        0.0
    );
}

#[tokio::test]
async fn msh_72_an_h2_only_client_is_answered_over_h2_and_reaches_the_pods() {
    let _logs = capture_logs();
    let (pod_ca, pod_root) = authority("pods");
    let a = tls_pod("a", &pod_ca, names(&[NAME]), &[b"h2"]).await;
    let b = tls_pod("b", &pod_ca, names(&[NAME]), &[b"h2"]).await;
    let rig = rig(&pod_root, cluster(), &[a.addr, b.addr]).await;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let tls = client(&rig, service(), &[b"h2"]).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, http_body_util::Empty<Bytes>>(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(conn);
        let request = Request::get(format!("https://{NAME}/a"))
            .body(http_body_util::Empty::new())
            .unwrap();
        let response = send.send_request(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        seen.insert(String::from_utf8(body.to_vec()).unwrap());
    }
    assert_eq!(seen.len(), 2, "both pods answered: {seen:?}");
}

/// A request over a rig whose pods and ClusterIP are all given the certificates `bad` says.
async fn refused_by(logs: &common::Logs, rig: &Rig, pods: &[&TlsPod], extra: &[&TlsPod]) -> String {
    let mut tls = client(rig, service(), &[]).await;
    let (status, _) = get(&mut tls).await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    for pod in pods.iter().chain(extra) {
        assert_eq!(pod.requests.load(Ordering::SeqCst), 0, "pod {}", pod.name);
    }
    logs.settled().await;
    logs.text()
}

#[tokio::test]
async fn msh_73_a_pod_whose_certificate_does_not_verify_gets_no_byte_and_the_answer_is_503() {
    let logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let (bad_ca, _) = authority("unknown");
    // Signed by an unknown CA, naming another Service, naming only the IP.
    let unknown = tls_pod("unknown", &bad_ca, names(&[NAME]), &[]).await;
    let other = tls_pod("other", &good_ca, names(&["pay.shop.svc"]), &[]).await;
    let ip_only = tls_pod(
        "ip",
        &good_ca,
        vec![SanType::IpAddress("127.0.0.1".parse().unwrap())],
        &[],
    )
    .await;
    let clusterpod = tls_pod("cluster", &bad_ca, names(&[NAME]), &[]).await;
    for bad in [&unknown, &other, &ip_only] {
        let rig = rig(&good_root, clusterpod.addr, &[bad.addr]).await;
        let text = refused_by(&logs, &rig, &[bad], &[&clusterpod]).await;
        assert!(
            text.contains("tls verification failed"),
            "the failed verification is logged"
        );
        assert!(text.contains("tls_error"), "the TLS error is a field");
        assert!(
            counter(&rig.metrics.encode(), "mimic_mesh_fallbacks_total") >= 1.0,
            "the original destination was tried"
        );
    }
}

#[tokio::test]
async fn msh_73_with_one_good_pod_and_one_bad_pod_every_request_is_answered_by_the_good_one() {
    let _logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let (bad_ca, _) = authority("unknown");
    let good = tls_pod("good", &good_ca, names(&[NAME]), &[]).await;
    let bad = tls_pod("bad", &bad_ca, names(&[NAME]), &[]).await;
    let rig = rig(&good_root, cluster(), &[bad.addr, good.addr]).await;
    for _ in 0..20 {
        let mut tls = client(&rig, service(), &[]).await;
        let (status, body) = get(&mut tls).await;
        assert_eq!(
            (status.as_str(), body.as_str()),
            ("HTTP/1.1 200 OK", "good")
        );
    }
    assert_eq!(bad.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn msh_74_an_upgrade_over_intercepted_tls_reaches_a_pod_over_tls() {
    let _logs = capture_logs();
    let (pod_ca, pod_root) = authority("pods");
    let pod = tls_pod("a", &pod_ca, names(&[NAME]), &[]).await;
    let rig = rig(&pod_root, cluster(), &[pod.addr]).await;
    let mut tls = client(&rig, service(), &[]).await;
    let (status, _) = get(&mut tls).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let request =
        format!("GET /ws HTTP/1.1\r\nHost: {NAME}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n");
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(TIMEOUT, tls.read(&mut buf))
            .await
            .expect("a 101 in time")
            .unwrap();
        assert!(n > 0, "closed: {got:?}");
        got.extend_from_slice(&buf[..n]);
    }
    assert!(String::from_utf8_lossy(&got).starts_with("HTTP/1.1 101"));
    tls.write_all(b"ping-over-tls").await.unwrap();
    let mut echoed = vec![0u8; 13];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut echoed))
        .await
        .expect("an echo in time")
        .unwrap();
    assert_eq!(echoed, b"ping-over-tls");
}

#[tokio::test]
async fn msh_74_a_decrypted_non_http_protocol_is_relayed_over_tls_to_a_pod() {
    let _logs = capture_logs();
    let (pod_ca, pod_root) = authority("pods");
    let pod = tls_pod("a", &pod_ca, names(&[NAME]), &[]).await;
    let rig = rig(&pod_root, cluster(), &[pod.addr]).await;
    let mut tls = client(&rig, service(), &[]).await;
    let bytes = b"\x00\x01binary\xff\x00protocol";
    tls.write_all(bytes).await.unwrap();
    let mut echoed = vec![0u8; bytes.len()];
    tokio::time::timeout(TIMEOUT, tls.read_exact(&mut echoed))
        .await
        .expect("an echo in time")
        .unwrap();
    assert_eq!(echoed, bytes);
    assert_eq!(pod.sni.lock().unwrap().as_slice(), [Some(NAME.to_string())]);
}

#[tokio::test]
async fn msh_74_a_client_that_rejects_the_leaf_is_remembered_and_its_next_connection_is_l4() {
    let _logs = capture_logs();
    let (pod_ca, pod_root) = authority("pods");
    let pod = tls_pod("a", &pod_ca, names(&[NAME]), &[]).await;
    let rig = rig(&pod_root, cluster(), &[pod.addr]).await;
    // This client trusts the pods' CA, not the mimic root.
    let direct = connector(&pod_root, &[]);
    let first = TcpStream::connect(rig.addr).await.unwrap();
    assert!(direct.connect(service(), first).await.is_err());
    let mut tls = None;
    for _ in 0..50 {
        let tcp = TcpStream::connect(rig.addr).await.unwrap();
        if let Ok(Ok(connected)) =
            tokio::time::timeout(TIMEOUT, direct.connect(service(), tcp)).await
        {
            tls = Some(connected);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut tls = tls.expect("the next connection reaches a pod untouched");
    let (status, body) = get(&mut tls).await;
    assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "a"));
}

#[tokio::test]
async fn rty_17_a_pod_that_does_not_verify_is_a_connect_failure_and_verified_pods_answer() {
    let _logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let (bad_ca, _) = authority("unknown");
    let bad = tls_pod("bad", &bad_ca, names(&[NAME]), &[]).await;
    let one = tls_pod("one", &good_ca, names(&[NAME]), &[]).await;
    let two = tls_pod("two", &good_ca, names(&[NAME]), &[]).await;
    let rig = rig(&good_root, cluster(), &[bad.addr, one.addr, two.addr]).await;
    for _ in 0..20 {
        let mut tls = client(&rig, service(), &[]).await;
        let (status, body) = get(&mut tls).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            ["one", "two"].contains(&body.as_str()),
            "a verified pod answered"
        );
    }
    assert_eq!(bad.requests.load(Ordering::SeqCst), 0);
    let scrape = rig.metrics.encode();
    assert!(
        counter(
            &scrape,
            "mimic_mesh_attempts_total{backend=\"shop/cart:80\",result=\"connect_failed\"}"
        ) >= 1.0,
        "{scrape}"
    );
    assert_eq!(counter(&scrape, "mimic_mesh_fallbacks_total"), 0.0);
}

#[tokio::test]
async fn rty_17_a_decrypted_non_http_connection_skips_a_pod_that_does_not_verify() {
    let _logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let (bad_ca, _) = authority("unknown");
    let bad = tls_pod("bad", &bad_ca, names(&[NAME]), &[]).await;
    let good = tls_pod("good", &good_ca, names(&[NAME]), &[]).await;
    let rig = rig(&good_root, cluster(), &[bad.addr, good.addr]).await;
    let bytes = b"\x00\x01binary\xff\x00protocol";
    for _ in 0..10 {
        let mut tls = client(&rig, service(), &[]).await;
        tls.write_all(bytes).await.unwrap();
        let mut echoed = vec![0u8; bytes.len()];
        tokio::time::timeout(TIMEOUT, tls.read_exact(&mut echoed))
            .await
            .expect("an echo in time")
            .expect("an echo");
        assert_eq!(echoed, bytes);
    }
    assert_eq!(
        good.sni.lock().unwrap().len(),
        10,
        "every one reached the good pod"
    );
}

#[tokio::test]
async fn rty_17_a_refusal_over_intercepted_tls_is_not_trusted() {
    let _logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let refuser = tls_pod("refuser", &good_ca, names(&[NAME]), &[]).await;
    let other = tls_pod("other", &good_ca, names(&[NAME]), &[]).await;
    let rig = rig(&good_root, cluster(), &[refuser.addr, other.addr]).await;
    let mut seen = 0;
    for _ in 0..10 {
        let mut tls = client(&rig, service(), &[]).await;
        let (status, _) = get(&mut tls).await;
        seen += usize::from(status == "HTTP/1.1 503 Service Unavailable");
    }
    assert!(seen >= 1, "the refusal reached the app");
    assert_eq!(
        refuser.requests.load(Ordering::SeqCst),
        seen,
        "every request the refuser saw was answered by it, none retried"
    );
    let scrape = rig.metrics.encode();
    assert_eq!(
        counter(&scrape, "mimic_mesh_retries_total"),
        0.0,
        "{scrape}"
    );
}

#[tokio::test]
async fn rty_37_an_app_refusal_over_intercepted_tls_is_not_trusted() {
    let _logs = capture_logs();
    let (good_ca, good_root) = authority("good");
    let refuser = tls_pod("apprefuser", &good_ca, names(&[NAME]), &[]).await;
    let other = tls_pod("other", &good_ca, names(&[NAME]), &[]).await;
    let rig = rig(&good_root, cluster(), &[refuser.addr, other.addr]).await;
    let mut seen = 0;
    for _ in 0..10 {
        let mut tls = client(&rig, service(), &[]).await;
        let (status, _) = get(&mut tls).await;
        seen += usize::from(status == "HTTP/1.1 503 Service Unavailable");
    }
    assert!(seen >= 1, "the answer reached the app");
    assert_eq!(
        refuser.requests.load(Ordering::SeqCst),
        seen,
        "none retried"
    );
    let scrape = rig.metrics.encode();
    assert_eq!(
        counter(&scrape, "mimic_mesh_retries_total"),
        0.0,
        "{scrape}"
    );
}
