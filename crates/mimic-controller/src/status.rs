//! `MimicWorkload` status (spec §7.4): every `MIMIC_STATUS_INTERVAL` the status of each
//! workload of the cache is computed from the store and applied when it differs.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use serde_json::Value;

use crate::store::Store;
use crate::workloads::WorkloadCache;

type Labels = Vec<(String, String)>;

/// `mimic_status_updates_total`.
#[derive(Clone, Default)]
pub struct StatusMetrics {
    updates: Family<Labels, Counter>,
}

impl StatusMetrics {
    pub fn register(registry: &mut Registry) -> StatusMetrics {
        let metrics = StatusMetrics::default();
        registry.register(
            "status_updates",
            "MimicWorkload status applies",
            metrics.updates.clone(),
        );
        metrics
    }
}

pub type ApplyFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Applies a `status.discovery` to a `MimicWorkload`.
pub trait StatusSink: Send + Sync {
    fn apply<'a>(&'a self, namespace: &'a str, name: &'a str, discovery: &'a Value)
    -> ApplyFuture<'a>;
}

pub struct Reconciler {
    pub store: Arc<Store>,
    pub cache: Arc<WorkloadCache>,
    pub sink: Arc<dyn StatusSink>,
    pub metrics: StatusMetrics,
    pub window: Duration,
    /// Whether `status not updated` was already logged for the current outage.
    pub down: AtomicBool,
}

impl Reconciler {
    /// One pass at `now_ms` (Unix milliseconds).
    pub async fn tick(&self, now_ms: u64) {
        let _ = now_ms;
        todo!("tick")
    }
}

#[cfg(test)]
mod tests;
