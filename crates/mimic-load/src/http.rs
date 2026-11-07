//! HTTP/1.1 towards the targets: the probe (spec §7.8 "Target check") and the load requests.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::client::conn::http1;
use hyper::header::{HeaderName, HeaderValue};
use hyper_util::rt::TokioIo;
use mimic_core::load::request::Request;
use mimic_core::load::summary::ErrorKind;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::config::RunId;

/// The sidecar admin port, on which `/load` answers.
pub const PROBE_PORT: u16 = 15100;
/// Time allowed to the whole probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_BODY_MAX: usize = 4096;
const MARKER_HEADER: &str = "x-mimic-load";

/// What the target's `x-mimic-load` header says of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    Ok,
    /// `refused; reason=<reason>`.
    Refused(String),
    /// Absent, or anything else.
    Missing,
}

impl Marker {
    pub fn parse(value: Option<&str>) -> Marker {
        let Some(value) = value.map(str::trim) else {
            return Marker::Missing;
        };
        if value == "ok" {
            return Marker::Ok;
        }
        match value.strip_prefix("refused") {
            Some(rest) => {
                let reason = rest
                    .split(';')
                    .filter_map(|part| part.trim().strip_prefix("reason="))
                    .next()
                    .unwrap_or_default();
                Marker::Refused(reason.trim().to_string())
            }
            None => Marker::Missing,
        }
    }
}

/// A kept-alive connection to one target port. Its driver task ends with it.
pub struct Conn {
    sender: http1::SendRequest<Full<Bytes>>,
    driver: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// A request ready to send, cheap to clone: a retry on a new connection reuses the body.
#[derive(Clone)]
pub struct Prepared {
    method: hyper::Method,
    uri: hyper::Uri,
    headers: hyper::HeaderMap,
    body: Bytes,
}

impl Prepared {
    /// `None` when a part is not valid HTTP/1.1.
    pub fn new(request: Request) -> Option<Prepared> {
        let mut headers = hyper::HeaderMap::new();
        for (name, value) in &request.headers {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).ok()?,
                HeaderValue::from_str(value).ok()?,
            );
        }
        Some(Prepared {
            method: hyper::Method::from_bytes(request.method.as_bytes()).ok()?,
            uri: request.target.parse().ok()?,
            headers,
            body: Bytes::from(request.body),
        })
    }

    fn build(&self) -> Option<hyper::Request<Full<Bytes>>> {
        let mut built = hyper::Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .body(Full::new(self.body.clone()))
            .ok()?;
        *built.headers_mut() = self.headers.clone();
        Some(built)
    }
}

/// How a request ended.
#[derive(Debug)]
pub enum Outcome {
    Response {
        status: u16,
        /// From the first byte written to the response head.
        micros: u64,
        marker: Marker,
        /// The body was read to its end and the connection is open.
        reusable: bool,
    },
    Error(ErrorKind),
}

fn kind_of(error: &hyper::Error) -> ErrorKind {
    if error.is_parse() || error.is_parse_status() || error.is_user() {
        ErrorKind::Protocol
    } else {
        ErrorKind::Reset
    }
}

impl Conn {
    pub async fn connect(addr: SocketAddr, timeout: Duration) -> Result<Conn, ErrorKind> {
        let tcp = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| ErrorKind::Timeout)?
            .map_err(|_| ErrorKind::Connect)?;
        let _ = tcp.set_nodelay(true);
        let (sender, connection) = http1::handshake(TokioIo::new(tcp))
            .await
            .map_err(|_| ErrorKind::Connect)?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Conn { sender, driver })
    }

    /// The connection is still usable for another request.
    pub fn alive(&self) -> bool {
        !self.sender.is_closed()
    }

    /// Sends `request` and reads the response, within `timeout` as a whole.
    pub async fn send(&mut self, request: &Prepared, timeout: Duration) -> Outcome {
        let Some(built) = request.build() else {
            return Outcome::Error(ErrorKind::Protocol);
        };
        let started = Instant::now();
        let exchange = async {
            let response = self.sender.send_request(built).await?;
            let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            let status = response.status().as_u16();
            let marker = Marker::parse(
                response
                    .headers()
                    .get(MARKER_HEADER)
                    .and_then(|v| v.to_str().ok()),
            );
            let mut body = response.into_body();
            // The body is read to its end and dropped, frame by frame.
            let mut complete = true;
            while let Some(frame) = body.frame().await {
                if frame.is_err() {
                    complete = false;
                    break;
                }
            }
            Ok::<_, hyper::Error>((status, micros, marker, complete))
        };
        match tokio::time::timeout(timeout, exchange).await {
            Err(_) => Outcome::Error(ErrorKind::Timeout),
            Ok(Err(error)) => Outcome::Error(kind_of(&error)),
            Ok(Ok((status, micros, marker, complete))) => Outcome::Response {
                status,
                micros,
                marker,
                reusable: complete && !self.sender.is_closed(),
            },
        }
    }
}

#[derive(Deserialize)]
struct Identity {
    deployment: String,
    mode: String,
    namespace: String,
    pod: String,
}

/// Why a target is not verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unverified {
    NoAnswer,
    Status,
    Invalid,
    Namespace,
    Deployment,
    Pod,
    Mode,
}

impl Unverified {
    pub fn as_str(self) -> &'static str {
        match self {
            Unverified::NoAnswer => "no answer",
            Unverified::Status => "status",
            Unverified::Invalid => "invalid answer",
            Unverified::Namespace => "namespace",
            Unverified::Deployment => "deployment",
            Unverified::Pod => "pod",
            Unverified::Mode => "mode",
        }
    }
}

/// Checks a probe answer against the run and the target's pod name.
pub fn verify(
    status: u16,
    body: &[u8],
    run: &RunId,
    deployment: &str,
    pod: &str,
) -> Result<(), Unverified> {
    if status != 200 {
        return Err(Unverified::Status);
    }
    let identity: Identity = serde_json::from_slice(body).map_err(|_| Unverified::Invalid)?;
    if identity.namespace != run.namespace {
        return Err(Unverified::Namespace);
    }
    if identity.deployment != deployment {
        return Err(Unverified::Deployment);
    }
    if identity.pod != pod {
        return Err(Unverified::Pod);
    }
    if identity.mode != "mock" {
        return Err(Unverified::Mode);
    }
    Ok(())
}

/// `GET http://<ip>:15100/load`, on a connection of its own.
pub async fn probe(ip: IpAddr, run: &RunId, deployment: &str, pod: &str) -> Result<(), Unverified> {
    let exchange = async {
        let addr = SocketAddr::new(ip, PROBE_PORT);
        let tcp = TcpStream::connect(addr).await.ok()?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(tcp)).await.ok()?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::builder()
            .uri("/load")
            .header("host", addr.to_string())
            .header("connection", "close")
            .body(Empty::<Bytes>::new())
            .ok()?;
        let response = sender.send_request(request).await.ok()?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(Ok(frame)) = body.frame().await {
            if let Some(data) = frame.data_ref() {
                if bytes.len() + data.len() > PROBE_BODY_MAX {
                    break;
                }
                bytes.extend_from_slice(data);
            }
        }
        driver.abort();
        Some((status, bytes))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, exchange).await {
        Ok(Some((status, body))) => verify(status, &body, run, deployment, pod),
        _ => Err(Unverified::NoAnswer),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId {
            namespace: "shop".into(),
            name: "t".into(),
            uid: "abc".into(),
        }
    }

    #[test]
    fn the_marker_header_is_read_strictly() {
        assert_eq!(Marker::parse(Some("ok")), Marker::Ok);
        assert_eq!(
            Marker::parse(Some("refused; reason=mode")),
            Marker::Refused("mode".into())
        );
        assert_eq!(
            Marker::parse(Some("refused; reason=ticket")),
            Marker::Refused("ticket".into())
        );
        assert_eq!(
            Marker::parse(Some("refused")),
            Marker::Refused(String::new())
        );
        assert_eq!(Marker::parse(Some("okay")), Marker::Missing);
        assert_eq!(Marker::parse(None), Marker::Missing);
    }

    #[test]
    fn a_probe_answer_must_name_the_run_the_pod_and_mock() {
        let body = |ns: &str, dep: &str, pod: &str, mode: &str| {
            format!(r#"{{"namespace":"{ns}","deployment":"{dep}","pod":"{pod}","mode":"{mode}"}}"#)
        };
        let check = |status, body: &str| verify(status, body.as_bytes(), &run(), "pay", "pay-1");
        assert_eq!(check(200, &body("shop", "pay", "pay-1", "mock")), Ok(()));
        assert_eq!(check(404, "{}"), Err(Unverified::Status));
        assert_eq!(check(200, "nope"), Err(Unverified::Invalid));
        assert_eq!(
            check(200, &body("other", "pay", "pay-1", "mock")),
            Err(Unverified::Namespace)
        );
        assert_eq!(
            check(200, &body("shop", "bill", "pay-1", "mock")),
            Err(Unverified::Deployment)
        );
        assert_eq!(
            check(200, &body("shop", "pay", "pay-2", "mock")),
            Err(Unverified::Pod)
        );
        assert_eq!(
            check(200, &body("shop", "pay", "pay-1", "discovery")),
            Err(Unverified::Mode)
        );
    }
}
