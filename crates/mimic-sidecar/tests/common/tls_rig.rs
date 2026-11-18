//! An outbound listener with §5 interception (the CA set-up of MSH-72) in front of a silent TLS
//! pod, for the tests of intercepted mesh-routed requests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::listener::{Direction, OriginalDst, Shared, TunnelMetrics, spawn_listener};
use mimic_sidecar::mesh::MeshState;
use mimic_sidecar::red::ProxyMetrics;
use mimic_sidecar::tls::ca::{CaMetrics, CaState};
use mimic_sidecar::tls::intercept::{Interception, TlsMetrics};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use super::mesh_pods::cluster_ip;

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

/// A pod serving TLS that reads and never answers; what it read is kept.
pub struct SilentTls {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<u8>>>,
}

async fn silent_tls(ca: &Authority) -> SilentTls {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = ["cart.shop.svc", "other.shop.svc"]
        .iter()
        .map(|n| SanType::DnsName((*n).try_into().unwrap()))
        .collect();
    let cert = params.signed_by(&key, ca).unwrap();
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pod = SilentTls {
        addr: listener.local_addr().unwrap(),
        received: Arc::default(),
    };
    let received = pod.received.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, received) = (acceptor.clone(), received.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                while let Ok(n) = tls.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    received.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });
        }
    });
    pod
}

pub struct Rig {
    pub addr: SocketAddr,
    pub root: CertificateDer<'static>,
    pub metrics: Arc<Metrics>,
    pub pod: SilentTls,
    _trust: tempfile::TempDir,
}

/// The outbound listener with §5 interception and the mesh state `state_for` makes of the silent
/// TLS pod's address; the app's original destination is `cluster_ip()`.
pub async fn rig_with(state_for: impl FnOnce(SocketAddr) -> MeshState) -> Rig {
    let (pod_ca, pod_root) = authority("pods");
    let pod = silent_tls(&pod_ca).await;
    let trust = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(trust.path().join("trust/0")).unwrap();
    std::fs::write(trust.path().join("trust/0/image-ca.pem"), pem(&pod_root)).unwrap();
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
    let state = state_for(pod.addr);
    state.register(&mut metrics);
    let original: OriginalDst = Arc::new(move |_| Ok(cluster_ip()));
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
        pod,
        _trust: trust,
    }
}

pub async fn client(rig: &Rig) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut roots = RootCertStore::empty();
    roots.add(rig.root.clone()).unwrap();
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("cart.shop.svc").unwrap(), tcp)
        .await
        .expect("handshake")
}

pub use tokio::net::TcpStream;
