//! Egress pull client (spec §7.7): stub, replaced by the implementation.

use std::future::Future;
use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;

use crate::discovery::Target;

#[derive(Debug, Clone)]
pub struct EgressConfig {
    pub interval: Duration,
    pub max_bytes: usize,
    pub timeout: Duration,
    pub target: Option<Target>,
}

impl EgressConfig {
    pub fn from_env(_env: &Env) -> Result<EgressConfig, ConfigError> {
        Ok(EgressConfig {
            interval: Duration::from_secs(60),
            max_bytes: 4 << 20,
            timeout: Duration::from_secs(30),
            target: None,
        })
    }
}

#[derive(Clone, Default)]
pub struct Snapshot;

impl Snapshot {
    pub fn etag(&self) -> Option<String> {
        None
    }
    pub fn endpoints(&self) -> usize {
        0
    }
    pub fn bytes(&self) -> usize {
        0
    }
    pub fn pulled(&self) -> Option<std::time::SystemTime> {
        None
    }
}

#[derive(Clone, Default)]
pub struct EgressMetrics;

impl EgressMetrics {
    pub fn register(&self, _metrics: &mut Metrics) {}
}

pub struct Puller;

impl Puller {
    pub fn new(
        _config: EgressConfig,
        _metrics: EgressMetrics,
        _snapshot: Snapshot,
    ) -> Result<Option<Puller>, String> {
        Ok(Some(Puller))
    }

    pub async fn run(self, stop: impl Future<Output = ()>) {
        stop.await;
    }
}
