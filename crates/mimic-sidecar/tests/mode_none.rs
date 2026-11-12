//! NON-03 and NON-04: a sidecar in `none` (spec §3, §5, §6), in process: it forwards, records
//! nothing and intercepts no TLS, and follows a pulled switch to `discovery` and back. Every wait
//! has a deadline.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::mock_rig::{Dest, Options, Rig, TlsSetup, rig_with, users_set};
use mimic_core::Mode;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig};
use mimic_sidecar::listener::Direction;
use mimic_sidecar::mode::Announced;
use mimic_sidecar::tls::ca::{CaMetrics, CaState};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

const TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST: &[u8] = b"GET /users/1 HTTP/1.1\r\nhost: users.test:8080\r\n\r\n";
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

/// A sidecar in `mode` whose destination answers every connection's first bytes with `OK`.
async fn rig(mode: Mode, direction: Direction, discovery: &Discovery) -> Rig {
    rig_with(
        users_set(5),
        100,
        16,
        None,
        Options {
            mode: Some(mode),
            direction: Some(direction),
            discovery: Some(discovery.tap()),
            dest: Dest::Hold {
                greeting: b"",
                reply: OK,
            },
            ..Options::default()
        },
    )
    .await
}

/// One request on a new connection; the response read to its end.
async fn call(rig: &Rig, request: &[u8]) -> String {
    let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
    tcp.write_all(request).await.unwrap();
    let mut out = vec![0u8; OK.len()];
    tokio::time::timeout(TIMEOUT, tcp.read_exact(&mut out))
        .await
        .expect("answered in time")
        .unwrap();
    String::from_utf8(out).unwrap()
}

fn empty(discovery: &Discovery) -> bool {
    let report = discovery.report();
    report.exchanges() == 0
        && report.to_json() == mimic_core::discovery::report::Report::new(None).to_json()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_03_none_forwards_outbound_and_inbound_and_records_nothing() {
    for direction in [Direction::Outbound, Direction::Inbound] {
        let discovery = Discovery::start(&DiscoveryConfig::default(), None).unwrap();
        let rig = rig(Mode::None, direction, &discovery).await;
        for _ in 0..3 {
            assert_eq!(call(&rig, REQUEST).await.as_bytes(), OK);
        }
        // Forwarded byte for byte, one connection each.
        assert_eq!(rig.accepted.load(Ordering::SeqCst), 3);
        let expected = REQUEST.repeat(3);
        assert_eq!(*rig.received.lock().unwrap(), expected);
        // Non-HTTP bytes are relayed as a tunnel, and recorded as none.
        let mut tcp = TcpStream::connect(rig.addr).await.unwrap();
        tcp.write_all(b"PING\r\n").await.unwrap();
        let mut out = vec![0u8; OK.len()];
        tokio::time::timeout(TIMEOUT, tcp.read_exact(&mut out))
            .await
            .expect("answered in time")
            .unwrap();
        drop(tcp);
        // The recorder works on its own thread: give it every chance to record.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(empty(&discovery), "recorded in none ({direction:?})");
        assert_eq!(discovery.tap().budget().used(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_05_a_pulled_switch_starts_and_stops_recording_without_a_restart() {
    let discovery = Discovery::start(&DiscoveryConfig::default(), None).unwrap();
    let rig = rig(Mode::None, Direction::Inbound, &discovery).await;
    assert_eq!(call(&rig, REQUEST).await.as_bytes(), OK);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(empty(&discovery));

    switch(&rig, Mode::Discovery).await;
    assert_eq!(call(&rig, REQUEST).await.as_bytes(), OK);
    eventually("the exchange is recorded", || {
        discovery.report().exchanges() == 1
    })
    .await;

    // Back to none: the next exchanges are not recorded, what was recorded stays for shipping.
    switch(&rig, Mode::None).await;
    for _ in 0..2 {
        assert_eq!(call(&rig, REQUEST).await.as_bytes(), OK);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(discovery.report().exchanges(), 1);
}

fn ca() -> (
    Arc<CaState>,
    tokio_rustls::rustls::pki_types::CertificateDer<'static>,
) {
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

/// A TLS client that trusts the mimic root and offers `api.test`; it never gets an answer from
/// the silent destination, so the handshake is abandoned after `wait`.
async fn hello(rig: &Rig, root: &tokio_rustls::rustls::pki_types::CertificateDer<'static>) {
    let mut roots = RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = TcpStream::connect(rig.addr).await.unwrap();
    let connector = TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from("api.test").unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(400), connector.connect(name, tcp)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_04_none_passes_outbound_tls_through_and_discovery_intercepts_again() {
    let (state, root) = ca();
    let discovery = Discovery::start(&DiscoveryConfig::default(), None).unwrap();
    let rig = rig_with(
        users_set(5),
        100,
        16,
        None,
        Options {
            mode: Some(Mode::None),
            discovery: Some(discovery.tap()),
            tls: Some(TlsSetup {
                exclude: "",
                ca: Some(state),
            }),
            dest: Dest::Hold {
                greeting: b"",
                reply: b"",
            },
            ..Options::default()
        },
    )
    .await;
    hello(&rig, &root).await;
    // The destination got the client's ClientHello: a TLS handshake record, passed through.
    eventually("the destination received the hello", || {
        rig.received.lock().unwrap().len() > 100
    })
    .await;
    assert_eq!(rig.accepted.load(Ordering::SeqCst), 1);
    {
        let received = rig.received.lock().unwrap();
        assert_eq!(received[0], 0x16, "not a TLS handshake record");
        assert!(
            received.windows(8).any(|w| w == b"api.test"),
            "the SNI of the client is not in what the destination got"
        );
    }
    assert_eq!(
        rig.counter(r#"mimic_tls_connections_total{interception="mode_none"}"#),
        1
    );
    assert!(rig.metrics.encode().contains("mode_none"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(empty(&discovery));

    // Pulled to discovery: the next connection is not passed through for the mode any more.
    switch(&rig, Mode::Discovery).await;
    hello(&rig, &root).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.counter(r#"mimic_tls_connections_total{interception="mode_none"}"#),
        1,
        "{}",
        rig.metrics.encode()
    );
}
