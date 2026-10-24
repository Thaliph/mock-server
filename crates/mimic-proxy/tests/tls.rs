//! Interception TLS sortante : PRX-06, PRX-07 et détection de l'épinglage (spec §4.2).

mod common;

use std::time::Duration;

use common::*;
use mimic_proxy::Running;
use mimic_proxy::config::{Config, Mode};
use serde_json::Value;

struct Setup {
    proxy: Running,
    saas: Fake,
    saas_pem: String,
    ca_dir: tempfile::TempDir,
}

async fn setup(intercept: &[&str], mode: Mode) -> Setup {
    let (saas, saas_pem) = fake_https_server().await;
    let ca_dir = tempfile::tempdir().unwrap();
    let upstream_ca = ca_dir.path().join("upstream.pem");
    std::fs::write(&upstream_ca, &saas_pem).unwrap();
    let mut c: Config = config(None, mode);
    c.tls.intercept = intercept.iter().map(|s| s.to_string()).collect();
    c.tls.ca_dir = Some(ca_dir.path().join("out"));
    c.tls.upstream_ca_file = Some(upstream_ca);
    let proxy = common::proxy(c).await;
    Setup {
        proxy,
        saas,
        saas_pem,
        ca_dir,
    }
}

impl Setup {
    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.saas.addr.port())
    }

    /// Application configurée avec `HTTPS_PROXY` et le bundle publié par le sidecar.
    fn app(&self) -> reqwest::Client {
        let ca = std::fs::read(self.ca_dir.path().join("out/ca.crt")).unwrap();
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{}", self.proxy.outbound_addr())).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
            .build()
            .unwrap()
    }

    /// Application qui ne fait confiance qu'au vrai certificat de la dépendance.
    fn pinned_app(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{}", self.proxy.outbound_addr())).unwrap())
            .tls_certs_only([reqwest::Certificate::from_pem(self.saas_pem.as_bytes()).unwrap()])
            .build()
            .unwrap()
    }
}

#[tokio::test]
async fn ca_files_are_published_for_the_app() {
    let s = setup(&["localhost"], Mode::Record).await;
    let ca = std::fs::read_to_string(s.ca_dir.path().join("out/ca.crt")).unwrap();
    let bundle = std::fs::read_to_string(s.ca_dir.path().join("out/ca-bundle.crt")).unwrap();
    assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(bundle.ends_with(&ca));
}

#[tokio::test]
async fn prx_06_intercepted_https_is_recorded_then_mocked() {
    let s = setup(&["localhost"], Mode::Record).await;
    let app = s.app();
    let r = app.get(s.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "user3");

    let e = wait_exchanges(&s.proxy, "direction=outbound", 1)
        .await
        .remove(0);
    assert_eq!(e["scheme"], "https");
    assert_eq!(e["authority"], format!("localhost:{}", s.saas.addr.port()));
    assert_eq!(e["path"], "/users/3");

    let hits = s.saas.hits();
    s.saas.stop();
    set_mode(&s.proxy, "mock").await;
    let r = app.get(s.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mimic-match"));
    assert_eq!(r.json::<Value>().await.unwrap()["id"], 3);
    assert_eq!(s.saas.hits(), hits);
}

#[tokio::test]
async fn prx_07_other_hosts_are_tunnelled_blind() {
    let s = setup(&["api.other.example"], Mode::Record).await;
    let r = s.pinned_app().get(s.url("/users/1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exchanges(&s.proxy, "direction=outbound").await.is_empty());
}

#[tokio::test]
async fn pinned_client_rejection_is_counted() {
    let s = setup(&["localhost"], Mode::Record).await;
    assert!(s.pinned_app().get(s.url("/users/1")).send().await.is_err());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let m = metrics(&s.proxy).await;
        if m.contains("mimic_tls_handshake_failures_total{") {
            assert!(m.contains(r#"reason="client_rejected""#), "{m}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{m}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Dépendance gRPC en HTTPS (ALPN `h2` puis `http/1.1`) : un appel gRPC est renvoyé avec
/// ses trailers, un `GET` reçoit du JSON. Rend l'adresse, le certificat et le compteur.
async fn grpc_tls_server() -> (
    std::net::SocketAddr,
    String,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http_body_util::{BodyExt, StreamBody};
    use hyper::body::Frame;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls_pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    )
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, hits) = (acceptor.clone(), counter.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async move {
                            let version = format!("{:?}", req.version());
                            let grpc = req
                                .headers()
                                .get("content-type")
                                .is_some_and(|v| v == "application/grpc");
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let mut frames = vec![];
                            let mut builder =
                                hyper::Response::builder().header("x-upstream-version", version);
                            if grpc {
                                let mut trailers = hyper::HeaderMap::new();
                                trailers.insert("grpc-status", "0".parse().unwrap());
                                trailers.insert("grpc-message", "OK".parse().unwrap());
                                frames.push(Ok::<_, std::convert::Infallible>(Frame::data(body)));
                                frames.push(Ok(Frame::trailers(trailers)));
                                builder = builder.header("content-type", "application/grpc");
                            } else {
                                frames.push(Ok(Frame::data(bytes::Bytes::from_static(
                                    br#"{"ok":true}"#,
                                ))));
                                builder = builder.header("content-type", "application/json");
                            }
                            Ok::<_, std::convert::Infallible>(
                                builder
                                    .body(StreamBody::new(futures_util::stream::iter(frames)))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    (addr, cert.pem(), hits)
}

/// Réponse vue par un client qui passe par `CONNECT`, avec l'ALPN qu'il propose.
struct Seen {
    alpn: Option<Vec<u8>>,
    status: u16,
    headers: hyper::HeaderMap,
    body: Vec<u8>,
    trailers: Option<hyper::HeaderMap>,
}

/// Appel par le sidecar (`CONNECT`, puis TLS vers le certificat forgé) : gRPC en HTTP/2 si
/// `h2` est négocié, sinon `GET` en HTTP/1.1.
async fn call_through(p: &Running, ca_pem: &[u8], port: u16, alpn: &[&[u8]]) -> Seen {
    use std::sync::Arc;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = format!("localhost:{port}");
    let mut tcp = tokio::net::TcpStream::connect(p.outbound_addr())
        .await
        .unwrap();
    tcp.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = vec![];
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        tcp.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    let mut roots = rustls::RootCertStore::empty();
    use rustls_pki_types::pem::PemObject;
    for c in rustls_pki_types::CertificateDer::pem_slice_iter(ca_pem) {
        roots.add(c.unwrap()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect("localhost".try_into().unwrap(), tcp)
        .await
        .unwrap();
    let negotiated = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    let resp = if negotiated.as_deref() == Some(b"h2") {
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Full<Bytes>>(
            TokioExecutor::new(),
            TokioIo::new(tls),
        )
        .await
        .unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .method("POST")
            .uri(format!("https://{authority}/shop.Quotes/Get"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Full::new(Bytes::from_static(b"\0\0\0\0\x02hi")))
            .unwrap();
        sender.send_request(req).await.unwrap()
    } else {
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(tls))
                .await
                .unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .uri("/status")
            .header("host", &authority)
            .body(Full::new(Bytes::new()))
            .unwrap();
        sender.send_request(req).await.unwrap()
    };
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let collected = resp.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned();
    Seen {
        alpn: negotiated,
        status,
        headers,
        body: collected.to_bytes().to_vec(),
        trailers,
    }
}

/// gRPC sur HTTPS intercepté : HTTP/2 gardé, trailers relayés et enregistrés (PRX-25), puis
/// rejoués par le mock (MCK-28).
#[tokio::test]
async fn prx_25_mck_28_grpc_over_intercepted_tls_keeps_h2_and_trailers() {
    use std::sync::atomic::Ordering;
    let (addr, pem, hits) = grpc_tls_server().await;
    let dir = tempfile::tempdir().unwrap();
    let upstream_ca = dir.path().join("upstream.pem");
    std::fs::write(&upstream_ca, &pem).unwrap();
    let mut c: Config = config(None, Mode::Record);
    c.tls.intercept = vec!["localhost".into()];
    c.tls.ca_dir = Some(dir.path().join("out"));
    c.tls.upstream_ca_file = Some(upstream_ca);
    c.redact.raw_bodies = vec!["localhost".into()];
    let p = common::proxy(c).await;
    let ca = std::fs::read(dir.path().join("out/ca.crt")).unwrap();

    let seen = call_through(&p, &ca, addr.port(), &[b"h2", b"http/1.1"]).await;
    assert_eq!(seen.alpn.as_deref(), Some(&b"h2"[..]));
    assert_eq!(seen.status, 200);
    assert_eq!(seen.headers["x-upstream-version"], "HTTP/2.0");
    assert_eq!(seen.body, b"\0\0\0\0\x02hi");
    let trailers = seen.trailers.expect("trailers relayés");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["grpc-message"], "OK");

    let list = wait_exchanges(&p, "direction=outbound", 1).await;
    let e = decode(&list[0]);
    assert_eq!(e.path, "/shop.Quotes/Get");
    assert_eq!(
        e.resp_body,
        mimic_core::model::Body::captured(b"\0\0\0\0\x02hi".to_vec())
    );
    let names: Vec<(&str, &str)> = e
        .resp_trailers
        .iter()
        .map(|h| (h.name.as_str(), h.value.as_str()))
        .collect();
    assert_eq!(names, [("grpc-status", "0"), ("grpc-message", "OK")]);

    // Un client HTTP/1.1 seul reste en HTTP/1.1.
    let plain = call_through(&p, &ca, addr.port(), &[b"http/1.1"]).await;
    assert_eq!(plain.alpn.as_deref(), Some(&b"http/1.1"[..]));
    assert_eq!(plain.status, 200);
    assert_eq!(plain.body, br#"{"ok":true}"#);
    assert!(plain.trailers.is_none());

    // Mock : body et trailers rejoués, rien ne sort.
    set_mode(&p, "mock").await;
    let before = hits.load(Ordering::SeqCst);
    let mocked = call_through(&p, &ca, addr.port(), &[b"h2", b"http/1.1"]).await;
    assert_eq!(mocked.status, 200);
    assert!(
        mocked.headers.contains_key("x-mimic-match"),
        "{:?}",
        mocked.headers
    );
    assert_eq!(mocked.body, b"\0\0\0\0\x02hi");
    let trailers = mocked.trailers.expect("trailers rejoués");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(trailers["grpc-message"], "OK");
    // Sans trailers enregistrés : pas de trame de trailers.
    let plain = call_through(&p, &ca, addr.port(), &[b"http/1.1"]).await;
    assert_eq!(plain.status, 200);
    assert!(plain.trailers.is_none());
    assert_eq!(hits.load(Ordering::SeqCst), before);
}

fn decode(v: &Value) -> mimic_core::model::Exchange {
    serde_json::from_value(v.clone()).unwrap()
}
