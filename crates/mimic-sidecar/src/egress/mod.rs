//! The egress pull client of the sidecar (spec §7.7): pulls the deployment's aggregated
//! egress from the controller and holds it in memory, in every mode. F9 only holds,
//! measures and summarises it; nothing answers a request from it.

mod metrics;
mod pull;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mimic_core::discovery::egress::{self, EgressSet};
use mimic_runtime::config::{ConfigError, Env};

use crate::discovery::Target;

pub use metrics::EgressMetrics;
pub use pull::Puller;

const INTERVAL_VAR: &str = "MIMIC_EGRESS_PULL_INTERVAL";
const MAX_BYTES_VAR: &str = "MIMIC_EGRESS_MAX_BYTES";

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

/// Time allowed for one pull, the body included.
const PULL_TIMEOUT: Duration = Duration::from_secs(30);

/// Where and how often the egress set is pulled.
#[derive(Debug, Clone)]
pub struct EgressConfig {
    pub interval: Duration,
    /// Largest body read from the controller, in bytes.
    pub max_bytes: usize,
    /// Time allowed for one pull.
    pub timeout: Duration,
    /// `None` without `MIMIC_CONTROLLER_URL`: nothing is pulled.
    pub target: Option<Target>,
}

impl EgressConfig {
    pub fn from_env(env: &Env) -> Result<EgressConfig, ConfigError> {
        let interval = env.duration_or(INTERVAL_VAR, Duration::from_secs(60))?;
        if !(Duration::from_secs(1)..=Duration::from_secs(3600)).contains(&interval) {
            return Err(ConfigError::invalid(
                INTERVAL_VAR,
                env.optional(INTERVAL_VAR).unwrap_or_default(),
                "expected a duration from 1s to 1h",
            ));
        }
        Ok(EgressConfig {
            interval,
            max_bytes: size(env, MAX_BYTES_VAR, 4 * MIB)?,
            timeout: PULL_TIMEOUT,
            target: Target::from_env(env)?,
        })
    }
}

/// `<n>`, `<n>Ki`, `<n>Mi` or `<n>Gi`, from 64Ki to 1Gi.
fn size(env: &Env, var: &str, default: usize) -> Result<usize, ConfigError> {
    let Some(value) = env.optional(var) else {
        return Ok(default);
    };
    let (digits, unit) = match value {
        v if v.ends_with("Ki") => (&v[..v.len() - 2], KIB),
        v if v.ends_with("Mi") => (&v[..v.len() - 2], MIB),
        v if v.ends_with("Gi") => (&v[..v.len() - 2], GIB),
        v => (v, 1),
    };
    let bytes = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit));
    match bytes {
        Some(n) if (64 * KIB..=GIB).contains(&n) => Ok(n),
        _ => Err(ConfigError::invalid(
            var,
            value,
            "expected a size such as 4Mi (<n>, <n>Ki, <n>Mi or <n>Gi) from 64Ki to 1Gi",
        )),
    }
}

/// What the sidecar holds of the controller's egress set.
#[derive(Default)]
struct Held {
    set: Option<EgressSet>,
    etag: Option<String>,
    /// The last `200` or `304`.
    pulled: Option<SystemTime>,
    /// Size of the body the set was read from.
    bytes: usize,
}

/// The egress snapshot, shared between the pull client and its readers. Memory only: a
/// restarted sidecar pulls again.
#[derive(Clone, Default)]
pub struct Snapshot(Arc<Mutex<Held>>);

impl Snapshot {
    fn held(&self) -> MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `ETag` of the set held, `None` before the first `200`.
    pub fn etag(&self) -> Option<String> {
        self.held().etag.clone()
    }

    /// When the last `200` or `304` came.
    pub fn pulled(&self) -> Option<SystemTime> {
        self.held().pulled
    }

    pub fn endpoints(&self) -> usize {
        self.held()
            .set
            .as_ref()
            .map_or(0, EgressSet::endpoint_count)
    }

    /// Size of the body of the set held, 0 without one.
    pub fn bytes(&self) -> usize {
        self.held().bytes
    }

    /// Replaces the set. The old one is dropped here, after the new one was accepted and
    /// outside the lock.
    fn replace(&self, set: EgressSet, etag: Option<String>, bytes: usize, now: SystemTime) {
        let old = std::mem::replace(
            &mut *self.held(),
            Held {
                set: Some(set),
                etag,
                pulled: Some(now),
                bytes,
            },
        );
        drop(old);
    }

    /// The local view (spec §7.7): what the pod could mock, no sample and no field name.
    pub fn summary(&self) -> String {
        let held = self.held();
        let pulled = held.pulled.and_then(|at| {
            time::OffsetDateTime::from(at)
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
        match (&held.set, &pulled) {
            // A controller that sent no `ETag`: the set is held, but nothing names it.
            (Some(set), Some(pulled)) => {
                egress::summary(Some((set, held.etag.as_deref().unwrap_or(""), pulled)))
            }
            _ => egress::summary(None),
        }
    }

    /// `GET /egress`, for the admin listener. Answered to loopback peers only: the admin
    /// router must be served with connection info.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/egress", get(serve))
            .with_state(self.clone())
    }

    /// A `304`: the set is kept.
    fn touch(&self, now: SystemTime) {
        self.held().pulled = Some(now);
    }
}

async fn serve(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(snapshot): State<Snapshot>,
) -> Response {
    if !peer.ip().to_canonical().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            "egress is answered to loopback peers only\n",
        )
            .into_response();
    }
    // A large set must not stall the admin listener's worker.
    match tokio::task::spawn_blocking(move || snapshot.summary()).await {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
