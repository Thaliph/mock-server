//! The load marker of the inbound path and the `/load` probe (spec §7.8). A request that
//! carries `mimic-load-run` is forwarded only when its ticket verifies, names this pod's
//! namespace and deployment, and the mode is `mock`; the app never sees the header, and the
//! answer carries one `x-mimic-load` header. Anything else is answered `599` by the sidecar.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Bytes;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Response, StatusCode};
use mimic_core::Mode;
use mimic_core::load::ticket::{self, Payload};
use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

use crate::discovery::Tap;
use crate::mode::ModeState;
use crate::tls::TlsConfig;

/// The request header that carries the ticket.
pub const REQUEST_HEADER: &str = "mimic-load-run";
/// The answer header: `ok`, or `refused; reason=<reason>`.
pub const MARKER_HEADER: &str = "x-mimic-load";
/// Verified tickets kept, the last seen: a request costs a lookup.
const MAX_VERIFIED: usize = 16;
const ENABLED_VAR: &str = "MIMIC_LOAD_ENABLED";

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
    /// `None` unless `MIMIC_LOAD_ENABLED=1` (`0` or absent: load testing is off on this pod,
    /// spec §4.1). Else `MIMIC_POD_NAMESPACE`, `MIMIC_WORKLOAD` and `MIMIC_POD_NAME` (all set by
    /// the injection patch; without them no ticket names this pod) and the root of
    /// `MIMIC_TLS_CA`.
    pub fn from_env(env: &Env, tls: &TlsConfig) -> Result<Option<LoadConfig>, ConfigError> {
        match env.optional(ENABLED_VAR) {
            None | Some("0") => return Ok(None),
            Some("1") => {}
            Some(other) => {
                return Err(ConfigError::invalid(ENABLED_VAR, other, "expected 0 or 1"));
            }
        }
        let text = |name| env.optional(name).unwrap_or_default().to_string();
        Ok(Some(LoadConfig {
            namespace: text("MIMIC_POD_NAMESPACE"),
            deployment: text("MIMIC_WORKLOAD"),
            pod: text("MIMIC_POD_NAME"),
            root_der: tls.ca.as_ref().map(|ca| ca.root_der.clone()),
        }))
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
        let values: Vec<&[u8]> = headers
            .get_all(REQUEST_HEADER)
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        self.admit_values(&values)
    }

    /// [`Marker::admit`] for the values of the `mimic-load-run` headers of a request head.
    pub fn admit_values(&self, values: &[&[u8]]) -> Result<String, Refusal> {
        let decided = self.decide(values);
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
    fn decide(&self, values: &[&[u8]]) -> Result<String, Refusal> {
        let [value] = values else {
            return Err(Refusal::Ticket);
        };
        let payload = std::str::from_utf8(value)
            .ok()
            .map(str::trim)
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

/// What an inbound HTTP/1 relay needs of the load marker (spec §4.1): the check, the discovery
/// tap that counts the load exchanges, how long a head may stay incomplete, and the runs of the
/// requests read, in order, for the exchange tracker.
#[derive(Clone)]
pub struct Watch {
    pub(crate) marker: Marker,
    pub(crate) tap: Option<Tap>,
    pub(crate) stall: Duration,
    runs: Arc<Mutex<VecDeque<Option<String>>>>,
}

impl Watch {
    pub fn new(marker: Marker, tap: Option<Tap>, stall: Duration) -> Watch {
        Watch {
            marker,
            tap,
            stall,
            runs: Arc::default(),
        }
    }

    /// A request head was passed on: `Some(run)` when it carried an admitted ticket.
    pub(crate) fn pushed(&self, run: Option<String>) {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(run);
    }

    /// The run of the next request the codec reports.
    pub(crate) fn next_request(&self) -> Option<String> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .flatten()
    }
}
