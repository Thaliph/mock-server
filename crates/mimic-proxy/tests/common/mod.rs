//! Harnais des tests du proxy : application et dépendance factices, client, attentes.
#![allow(dead_code)]

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mimic_proxy::config::{Config, Mode};
use mimic_proxy::{Running, start};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Serveur HTTP factice qui compte ses requêtes.
pub struct Fake {
    pub addr: SocketAddr,
    pub hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Fake {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn stop(&self) {
        self.handle.abort();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn count(
    State(hits): State<Arc<AtomicUsize>>,
    req: axum::extract::Request,
) -> axum::extract::Request {
    hits.fetch_add(1, Ordering::SeqCst);
    req
}

pub async fn fake_server() -> Fake {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = router(hits.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fake { addr, hits, handle }
}

/// Même serveur factice en HTTPS, avec un certificat autosigné pour `localhost`.
/// Renvoie aussi le PEM de ce certificat, auquel les clients doivent se fier.
pub async fn fake_https_server() -> (Fake, String) {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let pem = cert.pem();
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
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let hits = Arc::new(AtomicUsize::new(0));
    let app = router(hits.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let svc = hyper_util::service::TowerToHyperService::new(app);
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(tls), svc)
                    .await;
            });
        }
    });
    (Fake { addr, hits, handle }, pem)
}

fn router(hits: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route(
            "/users/{id}",
            get(|Path(id): Path<u64>| async move {
                (
                    [("x-request-id", format!("req-{id}"))],
                    axum::Json(json!({"id": id, "name": format!("user{id}")})),
                )
            }),
        )
        .route(
            "/search",
            get(
                |axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
                    axum::Json(json!({"query": q.unwrap_or_default()}))
                },
            ),
        )
        .route(
            "/echo",
            post(|headers: HeaderMap, body: axum::body::Bytes| async move {
                let ct = headers.get(header::CONTENT_TYPE).cloned();
                let mut resp = Response::new(Body::from(body));
                if let Some(ct) = ct {
                    resp.headers_mut().insert(header::CONTENT_TYPE, ct);
                }
                resp
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                axum::Json(json!({"slow": true}))
            }),
        )
        .route(
            "/fail",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({"error": "rate limited"})),
                )
            }),
        )
        .route(
            "/gz",
            get(|| async {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(br#"{"compressed":true}"#).unwrap();
                (
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::CONTENT_ENCODING, "gzip"),
                    ],
                    enc.finish().unwrap(),
                )
            }),
        )
        .route(
            "/br",
            get(|| async {
                let mut out = Vec::new();
                brotli::CompressorWriter::new(&mut out, 4096, 5, 22)
                    .write_all(br#"{"compressed":"br"}"#)
                    .unwrap();
                (
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::CONTENT_ENCODING, "br"),
                    ],
                    out,
                )
            }),
        )
        .route(
            "/zstd",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::CONTENT_ENCODING, "zstd"),
                    ],
                    zstd::encode_all(&br#"{"compressed":"zstd"}"#[..], 3).unwrap(),
                )
            }),
        )
        .route(
            "/big",
            get(|| async {
                let chunk = bytes::Bytes::from(vec![b'x'; 64 * 1024]);
                let stream = futures_util::stream::iter(
                    (0..320).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
                );
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    Body::from_stream(stream),
                )
                    .into_response()
            }),
        )
        .layer(axum::middleware::map_request_with_state(hits, count))
}

/// Configuration de test : ports éphémères, application derrière `inbound`.
pub fn config(app: Option<SocketAddr>, mode: Mode) -> Config {
    let mut c = Config {
        mode,
        instance: "test-0".into(),
        ..Config::default()
    };
    c.workload.namespace = "ns".into();
    c.workload.name = "api".into();
    c.inbound.listen = "127.0.0.1:0".parse().unwrap();
    c.inbound.upstream = app;
    c.outbound.listen = "127.0.0.1:0".parse().unwrap();
    c.admin.listen = "127.0.0.1:0".parse().unwrap();
    c.redact.salt = Some("test".into());
    c
}

pub async fn proxy(config: Config) -> Running {
    start(config).await.expect("démarrage du proxy")
}

/// Client direct (sans proxy, y compris celui de l'environnement).
pub fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// Client dont le trafic sortant passe par le sidecar, comme une application avec `HTTP_PROXY`.
pub fn app_client(p: &Running) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", p.outbound_addr())).unwrap())
        .build()
        .unwrap()
}

pub fn admin(p: &Running, path: &str) -> String {
    format!("http://{}{path}", p.admin_addr())
}

pub fn inbound(p: &Running, path: &str) -> String {
    format!("http://{}{path}", p.inbound_addr())
}

pub async fn exchanges(p: &Running, query: &str) -> Vec<Value> {
    client()
        .get(admin(p, &format!("/exchanges?{query}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Attend que le store contienne au moins `n` échanges correspondant à `query`.
pub async fn wait_exchanges(p: &Running, query: &str, n: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = exchanges(p, query).await;
        if list.len() >= n {
            return list;
        }
        assert!(
            Instant::now() < deadline,
            "{} échanges sur {n} attendus",
            list.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn set_mode(p: &Running, mode: &str) {
    let r = client()
        .put(admin(p, "/mode"))
        .body(mode.to_string())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
}

pub async fn metrics(p: &Running) -> String {
    client()
        .get(admin(p, "/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// Ligne de métrique exacte, par exemple `mimic_mode{mode="mock"} 1`.
pub fn metric_value(text: &str, series: &str) -> Option<f64> {
    text.lines().find_map(|l| {
        l.strip_prefix(series)
            .and_then(|rest| rest.trim().parse().ok())
    })
}
