//! Endpoint discovery in the sidecar (spec §6): the capture tap of the HTTP/1 relay, the
//! bounded queue, the single recorder thread that owns the local report, the
//! `mimic_discovery_*` metrics and `GET /discovery` (loopback peers only). The inference, the
//! redaction and the merge are pure and live in `mimic_core::discovery`.

mod capture;
mod config;
mod decode;
mod metrics;
mod recorder;
mod redactions;

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::Report;
use mimic_runtime::metrics::Metrics;

pub use capture::Tap;
pub(crate) use capture::{Capture, Meta};
pub use config::DiscoveryConfig;
pub use metrics::DiscoveryMetrics;

/// Discovery of one sidecar: the local report and the thread that fills it.
pub struct Discovery {
    report: Arc<Mutex<Report>>,
    tap: Tap,
    metrics: DiscoveryMetrics,
}

impl Discovery {
    /// Starts the recorder thread. The thread logs through the subscriber of the caller and
    /// ends when the last [`Tap`] is dropped.
    pub fn start(config: &DiscoveryConfig, pod: Option<String>) -> io::Result<Discovery> {
        let report = Arc::new(Mutex::new(Report::new(pod)));
        let metrics = DiscoveryMetrics::default();
        let (queue, records) = sync_channel(config.queue);
        let tap = Tap::new(
            queue,
            Arc::new(capture::Budget::new(config.capture_budget)),
            metrics.clone(),
        );
        let recorder = recorder::Recorder::new(
            report.clone(),
            config.limits(),
            Redactor::new(config.redact_names.iter().map(String::as_str)),
            metrics.clone(),
        );
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name("mimic-discovery".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || recorder.run(records));
            })?;
        Ok(Discovery {
            report,
            tap,
            metrics,
        })
    }

    /// The handle the proxy captures exchanges with.
    pub fn tap(&self) -> Tap {
        self.tap.clone()
    }

    /// Exposes the `mimic_discovery_*` metrics in `metrics`.
    pub fn register(&self, metrics: &mut Metrics) {
        self.metrics.register(metrics);
    }

    /// A copy of the local report.
    pub fn report(&self) -> Report {
        self.report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `GET /discovery`, for the admin listener. Answered to loopback peers only: the admin
    /// router must be served with connection info.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/discovery", get(serve))
            .with_state(self.report.clone())
    }
}

async fn serve(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(report): State<Arc<Mutex<Report>>>,
) -> Response {
    if !peer.ip().to_canonical().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            "discovery is answered to loopback peers only\n",
        )
            .into_response();
    }
    // Serialising a large report must not stall the admin listener's worker.
    let json = tokio::task::spawn_blocking(move || {
        report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .to_json()
    })
    .await;
    answer(json, &SERIALISATION_LOGGED)
}

/// Set once the first serialisation failure was logged.
static SERIALISATION_LOGGED: AtomicBool = AtomicBool::new(false);

/// The response for a serialisation. A failure answers 500 and is logged once (`logged`),
/// with no part of the report in the line.
fn answer(
    json: Result<Result<String, serde_json::Error>, tokio::task::JoinError>,
    logged: &AtomicBool,
) -> Response {
    let error = match json {
        Ok(Ok(json)) => {
            return ([(header::CONTENT_TYPE, "application/json")], json).into_response();
        }
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    };
    if !logged.swap(true, Ordering::Relaxed) {
        tracing::error!(target: "discovery", error, "discovery serialisation failed");
    }
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsc_14_a_serialisation_failure_answers_500_and_is_logged_once() {
        let logged = AtomicBool::new(false);
        let failure = serde_json::from_str::<()>("x").unwrap_err();
        let response = answer(Ok(Err(failure)), &logged);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(logged.load(Ordering::Relaxed));
        let response = answer(Ok(Ok("{}".into())), &logged);
        assert_eq!(response.status(), StatusCode::OK);
    }
}
