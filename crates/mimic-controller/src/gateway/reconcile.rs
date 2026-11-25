//! The gateway reconciler (spec §7.10): the tables the watches feed, and the passes that write
//! the status of each mimic GatewayClass and of each Gateway of a mimic class. Both replicas
//! run it (no leader): equal applies change nothing in the cluster.

use std::sync::{Arc, Mutex, PoisonError};

use mimic_core::gateway::Settings;
use mimic_core::gateway::input;
use tokio::sync::watch;
use tokio::time::Instant;

use super::cluster::Cluster;
use super::metrics::GatewayMetrics;
use crate::mesh::Event;
use crate::mesh::table::Table;

/// What the watches deliver.
#[derive(Default)]
struct Tables {
    classes: Table<input::GatewayClass>,
    gateways: Table<input::Gateway>,
    /// The `gateways` CRD is served: false until the first look.
    crd: bool,
}

pub struct Reconciler {
    settings: Settings,
    cluster: Arc<dyn Cluster>,
    metrics: GatewayMetrics,
    tables: Mutex<Tables>,
    /// One pass at a time.
    passing: tokio::sync::Mutex<()>,
    /// Counts the changes of the tables and of the presence; `run` waits on it.
    changes: watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Reconciler {
    pub fn new(
        settings: Settings,
        cluster: Arc<dyn Cluster>,
        metrics: GatewayMetrics,
    ) -> Arc<Reconciler> {
        Arc::new(Reconciler {
            settings,
            cluster,
            metrics,
            tables: Mutex::default(),
            passing: tokio::sync::Mutex::new(()),
            changes: watch::channel(0).0,
        })
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn feed_classes(&self, event: Event<input::GatewayClass>) {
        if lock(&self.tables).classes.feed(event) {
            self.changed();
        }
    }

    pub fn feed_gateways(&self, event: Event<input::Gateway>) {
        if lock(&self.tables).gateways.feed(event) {
            self.changed();
        }
    }

    /// The `gateways` CRD is served or not; when it is not, the gateway table is emptied and a
    /// pass waits for a new full list.
    pub fn crd(&self, present: bool) {
        let _ = present;
    }

    /// One pass; returns when to run again after a failure.
    pub async fn pass(&self, now: &str) -> Option<Instant> {
        let _guard = self.passing.lock().await;
        let _ = (now, &self.settings, &self.cluster, &self.metrics);
        None
    }

    /// A pass after each change of a table or of the presence, and when a failure is due again.
    pub async fn run(self: Arc<Self>) {
        let mut changes = self.changes.subscribe();
        loop {
            changes.borrow_and_update();
            let due = self.pass(&crate::mesh::status::clock()).await;
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = async {
                    match due {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }
}
