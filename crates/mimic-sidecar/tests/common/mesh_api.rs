//! A fake sidecar API over TLS that answers `GET /v1/workloads/<d>/mesh` from a script (#442).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// What the fake API does with one request of the mesh route; any other route is `404`.
#[derive(Clone, Debug)]
pub enum Reply {
    /// `200` with this body and `ETag`.
    View(String, &'static str),
    /// `200` with the body gzip-compressed (`Content-Encoding: gzip`).
    Gzip(String, &'static str),
    NotModified,
    Status(u16),
    /// A status with `Retry-After: <n>`.
    RetryAfter(u16, u32),
    /// The connection is closed without an answer.
    Reset,
    /// No answer for an hour (a held long-poll).
    Hold,
}

pub struct Seen {
    pub at: Instant,
    pub target: String,
    pub authorization: String,
    pub if_none_match: Option<String>,
    pub accept_encoding: Option<String>,
}

pub struct MeshApi {
    pub addr: SocketAddr,
    pub ca_pem: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl MeshApi {
    /// Requests of the mesh route so far.
    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub fn at(&self, n: usize) -> Instant {
        self.seen.lock().unwrap()[n].at
    }

    pub fn if_none_match(&self, n: usize) -> Option<String> {
        self.seen.lock().unwrap()[n].if_none_match.clone()
    }

    pub fn authorization(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].authorization.clone()
    }

    pub fn target(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].target.clone()
    }

    pub fn accept_encoding(&self, n: usize) -> Option<String> {
        self.seen.lock().unwrap()[n].accept_encoding.clone()
    }
}

pub fn gzip(text: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(text.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

/// A fake API that follows `script`, then answers `fallback` for ever.
pub async fn mesh_api(script: Vec<Reply>, fallback: Reply) -> MeshApi {
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "controller ca".to_string());
    let ca_key = KeyPair::generate().unwrap();
    let ca_pem = ca_params.self_signed(&ca_key).unwrap().pem();
    let ca = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];
    let cert = params.signed_by(&key, &ca).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    )
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let script: Arc<Mutex<VecDeque<Reply>>> = Arc::new(Mutex::new(script.into()));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let seen_in = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, script, seen, fallback) = (
                acceptor.clone(),
                script.clone(),
                seen_in.clone(),
                fallback.clone(),
            );
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = service_fn(move |req: Request<Incoming>| {
                    let (script, seen, fallback) = (script.clone(), seen.clone(), fallback.clone());
                    async move {
                        if !req.uri().path().ends_with("/mesh") {
                            let body = Full::new(Bytes::new());
                            return Ok(Response::builder().status(404).body(body).unwrap());
                        }
                        let header = |name: &str| {
                            req.headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        seen.lock().unwrap().push(Seen {
                            at: Instant::now(),
                            target: req.uri().to_string(),
                            authorization: header("authorization").unwrap_or_default(),
                            if_none_match: header("if-none-match"),
                            accept_encoding: header("accept-encoding"),
                        });
                        let reply = script.lock().unwrap().pop_front().unwrap_or(fallback);
                        let response = Response::builder();
                        let (response, body) = match reply {
                            Reply::Reset => return Err("reset"),
                            Reply::Hold => {
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                return Err("held");
                            }
                            Reply::View(text, etag) => {
                                (response.status(200).header("etag", etag), Bytes::from(text))
                            }
                            Reply::Gzip(text, etag) => (
                                response
                                    .status(200)
                                    .header("etag", etag)
                                    .header("content-encoding", "gzip"),
                                Bytes::from(gzip(&text)),
                            ),
                            Reply::NotModified => (response.status(304), Bytes::new()),
                            Reply::Status(s) => (response.status(s), Bytes::new()),
                            Reply::RetryAfter(s, n) => (
                                response.status(s).header("retry-after", n.to_string()),
                                Bytes::new(),
                            ),
                        };
                        Ok(response.body(Full::new(body)).unwrap())
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    MeshApi { addr, ca_pem, seen }
}

/// A view document: Services `(key, ip, port)` with one endpoint each, the `mode`, the
/// `mesh` switch and the top-level `generations` (`mimicconfig/<ns>/<name>` → n).
pub fn view_json(
    services: &[(&str, &str, u16)],
    mode: &str,
    mesh: bool,
    generations: &[(&str, i64)],
) -> String {
    let services: Vec<String> = services
        .iter()
        .map(|(key, ip, port)| {
            format!(
                r#""{key}":{{"endpoints":{{"{port}":[{{"ip":"10.1.0.7","port":8080,"ready":true}}]}},"generations":{{"endpointslice/{key}-x":1}},"ips":["{ip}"],"policy":{{"loadBalancer":{{"slowStartSeconds":30}}}},"ports":{{"{port}":{{"targetPort":8080}}}}}}"#
            )
        })
        .collect();
    let generations: Vec<String> = generations
        .iter()
        .map(|(k, v)| format!(r#""{k}":{v}"#))
        .collect();
    format!(
        r#"{{"cluster":"local","format":1,"generations":{{{}}},"mesh":{mesh},"mode":"{mode}","services":{{{}}}}}"#,
        generations.join(","),
        services.join(",")
    )
}
