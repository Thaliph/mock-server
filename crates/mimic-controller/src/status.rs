//! `MimicWorkload` status (spec §7.4): every `MIMIC_STATUS_INTERVAL` the status of each
//! workload of the cache is computed from the store and applied when it differs.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams};
use mimic_core::discovery::status::discovery;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use serde_json::Value;

use tokio::sync::OnceCell;

use crate::store::{ReadError, Store};
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
    fn apply<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        discovery: &'a Value,
    ) -> ApplyFuture<'a>;
}

/// Server-side apply on the status subresource, as `mimic-controller`.
pub struct KubeSink {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeSink {
    /// `client` is set once the cluster client is available; applies fail until then.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeSink {
        KubeSink { client }
    }
}

impl StatusSink for KubeSink {
    fn apply<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        discovery: &'a Value,
    ) -> ApplyFuture<'a> {
        Box::pin(async move {
            let client = self.client.get().ok_or("no Kubernetes client yet")?;
            let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
                "mimic.platform.io",
                "v1alpha1",
                "MimicWorkload",
            ));
            let api = Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &resource);
            let body = serde_json::json!({
                "apiVersion": "mimic.platform.io/v1alpha1",
                "kind": "MimicWorkload",
                "metadata": {"name": name},
                "status": {"discovery": discovery},
            });
            api.patch_status(name, &PatchParams::apply(MANAGER), &Patch::Apply(&body))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

const MANAGER: &str = "mimic-controller";

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
        let window_ms = u64::try_from(self.window.as_millis()).unwrap_or(u64::MAX);
        // One read per deployment, however many workloads name it.
        let mut computed: BTreeMap<(String, String), Value> = BTreeMap::new();
        for target in self.cache.targets() {
            let key = (target.namespace.clone(), target.deployment.clone());
            let status = match computed.get(&key) {
                Some(status) => status.clone(),
                None => match self.store.status_input(&key.0, &key.1).await {
                    Ok((meta, pods)) => {
                        self.down.store(false, Ordering::SeqCst);
                        let status = discovery(meta.as_ref(), &pods, window_ms, now_ms);
                        computed.insert(key, status.clone());
                        status
                    }
                    Err(ReadError::Unavailable) => {
                        if !self.down.swap(true, Ordering::SeqCst) {
                            tracing::warn!("status not updated");
                        }
                        return;
                    }
                    // Logged by the store; the other workloads still get their status.
                    Err(_) => continue,
                },
            };
            if target.status.as_ref() == Some(&status) {
                continue;
            }
            let result = self
                .sink
                .apply(&target.namespace, &target.name, &status)
                .await;
            let label = if result.is_ok() { "ok" } else { "error" };
            self.metrics
                .updates
                .get_or_create(&vec![("result".to_string(), label.to_string())])
                .inc();
            if let Err(error) = result {
                tracing::warn!(
                    namespace = target.namespace,
                    workload = target.name,
                    error,
                    "status apply failed"
                );
            }
        }
    }

    /// Passes every `interval`, for ever.
    pub async fn run(self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            self.tick(now_ms).await;
        }
    }
}

#[cfg(test)]
mod tests;
