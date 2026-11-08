//! The load marker of the inbound path and the `/load` probe (spec §7.8). Skeleton: the
//! behaviour comes with the green commit.

use std::sync::Arc;

use axum::Router;
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;

use crate::mode::ModeState;
use crate::tls::TlsConfig;

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
    pub fn from_env(_env: &Env, _tls: &TlsConfig) -> LoadConfig {
        LoadConfig::default()
    }
}

#[derive(Clone)]
pub struct Marker(Arc<()>);

impl Marker {
    pub fn new(_config: LoadConfig, _mode: ModeState, _metrics: &mut Metrics) -> Marker {
        Marker(Arc::new(()))
    }

    /// `GET /load`, for the admin listener.
    pub fn router(&self) -> Router {
        Router::new()
    }
}
