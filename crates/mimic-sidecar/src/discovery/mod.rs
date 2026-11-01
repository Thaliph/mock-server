//! Endpoint discovery in the sidecar (spec §6). Stub: the red tests run against it.

use std::io;

use axum::Router;
use mimic_core::discovery::report::Report;
use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub max_endpoints: usize,
    pub max_literals: usize,
    pub sample_budget: usize,
    pub capture_budget: usize,
    pub queue: usize,
    pub redact_names: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        DiscoveryConfig {
            max_endpoints: 256,
            max_literals: 32,
            sample_budget: 16 << 20,
            capture_budget: 8 << 20,
            queue: 1024,
            redact_names: Vec::new(),
        }
    }
}

impl DiscoveryConfig {
    pub fn from_env(_env: &Env) -> Result<DiscoveryConfig, ConfigError> {
        Ok(DiscoveryConfig::default())
    }
}

#[derive(Clone)]
pub struct Tap;

pub struct Discovery;

impl Discovery {
    pub fn start(_config: &DiscoveryConfig, _pod: Option<String>) -> io::Result<Discovery> {
        Ok(Discovery)
    }

    pub fn tap(&self) -> Tap {
        Tap
    }

    pub fn register(&self, _metrics: &mut Metrics) {}

    pub fn report(&self) -> Report {
        Report::new(None)
    }

    pub fn router(&self) -> Router {
        Router::new()
    }
}
