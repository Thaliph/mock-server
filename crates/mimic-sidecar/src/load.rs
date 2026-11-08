//! The load marker of the inbound path and the `/load` probe (spec §7.8). A request that
//! carries `mimic-load-run` is forwarded only when its ticket verifies, names this pod's
//! namespace and deployment, and the mode is `mock`; the app never sees the header, and the
//! answer carries one `x-mimic-load` header. Anything else is answered `599` by the sidecar.

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use mimic_core::Mode;
use mimic_core::load::ticket::{self, Payload};
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

use crate::discovery::Tap;
use crate::exchange::{Ctx, ReqBody, Tracker};
use crate::mode::ModeState;
use crate::tls::TlsConfig;
use crate::transport::Stream;

/// The request header that carries the ticket.
pub const REQUEST_HEADER: &str = "mimic-load-run";
/// The answer header: `ok`, or `refused; reason=<reason>`.
pub const MARKER_HEADER: &str = "x-mimic-load";
/// Verified tickets kept, the last seen: a request costs a lookup.
const MAX_VERIFIED: usize = 16;

/// Who this sidecar is, and the key that signs the load tickets it accepts.
#[derive(Debug, Clone, Default)]
pub struct LoadConfig {
    pub namespace: String,
    pub deployment: String,
    pub pod: String,
    /// DER of the deployment root certificate (`MIMIC_TLS_CA`), none without one.
    pub root_der: Option<Vec<u8>>,
}

impl LoadConfig {
    /// `MIMIC_POD_NAMESPACE`, `MIMIC_WORKLOAD` and `MIMIC_POD_NAME` (all set by the injection
    /// patch; without them no ticket names this pod) and the root of `MIMIC_TLS_CA`.
    pub fn from_env(env: &Env, tls: &TlsConfig) -> LoadConfig {
        let text = |name| env.optional(name).unwrap_or_default().to_string();
        LoadConfig {
            namespace: text("MIMIC_POD_NAMESPACE"),
            deployment: text("MIMIC_WORKLOAD"),
            pod: text("MIMIC_POD_NAME"),
            root_der: tls.ca.as_ref().map(|ca| ca.root_der.clone()),
        }
    }
}

/// Why a marked request is not forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The mode is not `mock`.
    Mode,
    /// The ticket names another namespace or deployment.
    Workload,
    /// Malformed, badly signed, expired, or no `MIMIC_TLS_CA`.
    Ticket,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Mode => "mode",
            Refusal::Workload => "workload",
            Refusal::Ticket => "ticket",
        }
    }

    fn result(self) -> &'static str {
        match self {
            Refusal::Mode => "refused_mode",
            Refusal::Workload => "refused_workload",
            Refusal::Ticket => "refused_ticket",
        }
    }

    fn slot(self) -> usize {
        self as usize
    }

    /// The value of `x-mimic-load` that says it.
    pub fn header_value(self) -> HeaderValue {
        HeaderValue::from_str(&format!("refused; reason={}", self.as_str()))
            .unwrap_or_else(|_| HeaderValue::from_static("refused"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

struct Inner {
    config: LoadConfig,
    /// The public key of the root certificate (an uncompressed P-256 point).
    key: Option<Vec<u8>>,
    mode: ModeState,
    results: Family<ResultLabel, Counter>,
    verified: Mutex<VecDeque<(String, Payload)>>,
    /// The mode period (`ModeState::epoch`) in which each reason was last written.
    warned: Mutex<[Option<u64>; 3]>,
}

/// The load marker check of one sidecar, shared by its inbound connections and its admin
/// listener.
#[derive(Clone)]
pub struct Marker(Arc<Inner>);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The public key of a DER certificate, `None` when it does not parse.
fn public_key(der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    Some(cert.public_key().subject_public_key.data.to_vec())
}

impl Marker {
    /// Registers `mimic_load_inbound_total`.
    pub fn new(config: LoadConfig, mode: ModeState, metrics: &mut Metrics) -> Marker {
        let results = Family::<ResultLabel, Counter>::default();
        for refusal in [Refusal::Mode, Refusal::Workload, Refusal::Ticket] {
            let _ = results.get_or_create(&ResultLabel {
                result: refusal.result(),
            });
        }
        let _ = results.get_or_create(&ResultLabel {
            result: "forwarded",
        });
        metrics.registry_mut().register(
            "load_inbound",
            "Inbound requests carrying mimic-load-run, by result",
            results.clone(),
        );
        let key = config.root_der.as_deref().and_then(public_key);
        Marker(Arc::new(Inner {
            config,
            key,
            mode,
            results,
            verified: Mutex::new(VecDeque::new()),
            warned: Mutex::new([None; 3]),
        }))
    }

    /// Judges the `mimic-load-run` headers of a request, counts the result and, for a refusal,
    /// writes the warning of its reason once per mode period. `Ok` is the run's uid.
    pub fn admit(&self, headers: &HeaderMap) -> Result<String, Refusal> {
        let decided = self.decide(headers);
        let result = match decided {
            Ok(_) => "forwarded",
            Err(refusal) => refusal.result(),
        };
        self.0.results.get_or_create(&ResultLabel { result }).inc();
        if let Err(refusal) = decided {
            self.warn(refusal);
        }
        decided
    }

    /// Whether `headers` carry the marker at all.
    pub fn marked(headers: &HeaderMap) -> bool {
        headers.contains_key(REQUEST_HEADER)
    }

    /// Ticket first (a bad one says nothing about the pod), then the pod, then the mode.
    fn decide(&self, headers: &HeaderMap) -> Result<String, Refusal> {
        let mut values = headers.get_all(REQUEST_HEADER).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return Err(Refusal::Ticket);
        };
        let payload = value
            .to_str()
            .ok()
            .and_then(|ticket| self.verify(ticket))
            .ok_or(Refusal::Ticket)?;
        let config = &self.0.config;
        if payload.namespace != config.namespace || payload.deployment != config.deployment {
            return Err(Refusal::Workload);
        }
        if self.0.mode.mode() != Mode::Mock {
            return Err(Refusal::Mode);
        }
        Ok(payload.uid)
    }

    /// The payload of a ticket whose signature verifies with the root key and that has not
    /// expired.
    fn verify(&self, text: &str) -> Option<Payload> {
        let key = self.0.key.as_deref()?;
        let known = {
            let mut verified = self
                .0
                .verified
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let at = verified.iter().position(|(seen, _)| seen == text);
            at.and_then(|at| verified.remove(at)).map(|entry| {
                let payload = entry.1.clone();
                verified.push_back(entry);
                payload
            })
        };
        let payload = match known {
            Some(payload) => payload,
            None => {
                let parsed = ticket::parse(text)?;
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key)
                    .verify(&parsed.signed, &parsed.signature)
                    .ok()?;
                let mut verified = self
                    .0
                    .verified
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if verified.len() >= MAX_VERIFIED {
                    verified.pop_front();
                }
                verified.push_back((text.to_string(), parsed.payload.clone()));
                parsed.payload
            }
        };
        (payload.exp > now()).then_some(payload)
    }

    fn warn(&self, refusal: Refusal) {
        let period = self.0.mode.epoch();
        let first = {
            let mut warned = self.0.warned.lock().unwrap_or_else(PoisonError::into_inner);
            warned[refusal.slot()].replace(period) != Some(period)
        };
        if first {
            tracing::warn!(target: "load", reason = refusal.as_str(), "load request refused");
        }
    }

    /// `GET /load`, for the admin listener: who this pod is and its mode, for the generator's
    /// probe. Other methods answer `405`.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/load", get(identity))
            .with_state(self.clone())
    }
}

async fn identity(State(marker): State<Marker>) -> impl IntoResponse {
    let config = &marker.0.config;
    let body = serde_json::json!({
        "deployment": config.deployment,
        "mode": marker.0.mode.mode().as_str(),
        "namespace": config.namespace,
        "pod": config.pod,
    });
    ([(CONTENT_TYPE, "application/json")], body.to_string())
}

/// What the answer to a forwarded load request says: the app's own `x-mimic-load` headers go,
/// one `ok` stays.
pub fn mark_answer(headers: &mut HeaderMap) {
    headers.remove(MARKER_HEADER);
    headers.insert(MARKER_HEADER, HeaderValue::from_static("ok"));
}

/// The body of a forwarded or sidecar-made answer.
pub(crate) type Body = UnsyncBoxBody<Bytes, hyper::Error>;

/// The sidecar's own answer to a load request it does not forward (spec §7.8).
pub(crate) fn refused(refusal: Refusal) -> Response<Body> {
    let mut response = Response::new(
        http_body_util::Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::from_u16(599).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers_mut();
    headers.insert(MARKER_HEADER, refusal.header_value());
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// Judges the request headers when they carry the marker: `Ok(None)` for a plain request,
/// `Ok(Some(run))` for a load request admitted (its ticket removed, the exchange counted as
/// not recorded), the refusal otherwise.
pub(crate) fn screen(
    marker: &Marker,
    tap: Option<&Tap>,
    headers: &mut HeaderMap,
) -> Result<Option<String>, Refusal> {
    if !Marker::marked(headers) {
        return Ok(None);
    }
    let run = marker.admit(headers)?;
    headers.remove(REQUEST_HEADER);
    if let Some(tap) = tap {
        tap.load_exchange();
    }
    Ok(Some(run))
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Headers about the hop that sent them: a load request is rebuilt for the app's connection.
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// What the HTTP/1 service of a connection handed over by the relay shares between requests.
struct Http1 {
    marker: Marker,
    ctx: Ctx,
    tap: Option<Tap>,
    dst: SocketAddr,
    /// The connection to the app, opened by the first request and kept while it lives.
    app: tokio::sync::Mutex<Option<SendRequest<ReqBody>>>,
}

/// Serves the rest of an inbound HTTP/1 connection from the first request that carries the load
/// marker (`client` replays the bytes read): each request is checked, and forwarded to `dst`
/// over one connection kept for the client's. The relay is byte-exact; this is not, which is
/// why it only takes a connection that has shown a marker.
pub(crate) async fn serve_http1(
    marker: Marker,
    ctx: Ctx,
    tap: Option<Tap>,
    client: Stream,
    dst: SocketAddr,
    stop: impl Future<Output = ()>,
    abort: impl Future<Output = ()>,
) {
    let state = Arc::new(Http1 {
        marker,
        ctx,
        tap,
        dst,
        app: tokio::sync::Mutex::new(None),
    });
    let service = service_fn(move |req| forward(state.clone(), req));
    let served = hyper::server::conn::http1::Builder::new()
        .auto_date_header(false)
        .serve_connection(TokioIo::new(client), service);
    let mut served = pin!(served);
    let mut stop = pin!(stop);
    let mut abort = pin!(abort);
    let mut stopping = false;
    loop {
        tokio::select! {
            result = served.as_mut() => {
                if let Err(e) = result {
                    tracing::debug!(error = %e, "load connection ended");
                }
                return;
            }
            () = &mut stop, if !stopping => {
                stopping = true;
                served.as_mut().graceful_shutdown();
            }
            () = &mut abort => return,
        }
    }
}

async fn forward(
    state: Arc<Http1>,
    req: Request<Incoming>,
) -> Result<Response<Body>, Box<dyn std::error::Error + Send + Sync>> {
    let mut req = req;
    let run = match screen(&state.marker, state.tap.as_ref(), req.headers_mut()) {
        Ok(run) => run,
        Err(refusal) => return Ok(refused(refusal)),
    };
    if run.is_some() {
        for name in HOP_BY_HOP {
            req.headers_mut().remove(name);
        }
    }
    let tap = state.tap.as_ref().filter(|_| run.is_none());
    let (mut tracker, req) = Tracker::start(&state.ctx, "http1", tap, req);
    let marked = run.is_some();
    if let Some(run) = run {
        tracker.load_run(run);
    }
    let sent = async {
        let mut app = state.app.lock().await;
        let mut sender = match app.take() {
            Some(sender) if !sender.is_closed() => sender,
            _ => connect(state.dst).await?,
        };
        sender.ready().await?;
        let response = sender.send_request(req).await?;
        *app = Some(sender);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(response)
    }
    .await;
    match sent {
        Ok(response) => {
            let mut answered = tracker.answered(response).map(BodyExt::boxed_unsync);
            if marked {
                mark_answer(answered.headers_mut());
            }
            Ok(answered)
        }
        Err(e) => {
            tracker.failed();
            Err(e)
        }
    }
}

/// A new HTTP/1 connection to the app.
async fn connect(
    dst: SocketAddr,
) -> Result<SendRequest<ReqBody>, Box<dyn std::error::Error + Send + Sync>> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(dst)).await??;
    let (sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(Stream::plain(tcp))).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
}
