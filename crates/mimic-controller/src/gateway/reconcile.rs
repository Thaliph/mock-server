//! The gateway reconciler (spec §7.10): the tables the watches feed, and the passes that write
//! the status of each mimic GatewayClass and of each Gateway of a mimic class. Both replicas
//! run it (no leader): equal applies change nothing in the cluster.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mimic_core::gateway::input::{Gateway, GatewayClass, Observed};
use mimic_core::gateway::status::Plan;
use mimic_core::gateway::{Settings, accept, status};
use tokio::sync::watch;
use tokio::time::Instant;

use super::cluster::{Cluster, StatusTarget};
use super::metrics::{GatewayMetrics, STATES};
use crate::mesh::Event;
use crate::mesh::convert::Key;
use crate::mesh::table::Table;

/// A failed request is tried again after this long.
const RETRY: Duration = Duration::from_secs(30);

/// What the watches deliver.
#[derive(Default)]
struct Tables {
    classes: Table<GatewayClass>,
    gateways: Table<Gateway>,
    /// The `gateways` CRD is served: false until the first look.
    crd: bool,
}

/// What a pass keeps for the next one.
#[derive(Default)]
struct State {
    /// The state of each Gateway of a mimic class at its last pass, for the gauge.
    states: BTreeMap<Key, &'static str>,
}

pub struct Reconciler {
    settings: Settings,
    cluster: Arc<dyn Cluster>,
    metrics: GatewayMetrics,
    tables: Mutex<Tables>,
    /// Held for a whole pass: one at a time.
    state: tokio::sync::Mutex<State>,
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
            state: tokio::sync::Mutex::default(),
            changes: watch::channel(0).0,
        })
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    pub fn feed_classes(&self, event: Event<GatewayClass>) {
        if lock(&self.tables).classes.feed(event) {
            self.changed();
        }
    }

    pub fn feed_gateways(&self, event: Event<Gateway>) {
        if lock(&self.tables).gateways.feed(event) {
            self.changed();
        }
    }

    /// The `gateways` CRD is served or not; when it is not, the gateway table is emptied and a
    /// pass waits for a new full list (GW-15).
    pub fn crd(&self, present: bool) {
        let changed = {
            let mut tables = lock(&self.tables);
            let changed = tables.crd != present;
            tables.crd = present;
            if !present {
                tables.gateways = Table::default();
            }
            changed
        };
        if changed {
            self.changed();
        }
    }

    /// What a pass works on: the classes and the Gateways of mimic classes, or `None` while the
    /// picture is partial.
    fn picture(&self) -> Option<(Vec<GatewayClass>, Vec<Gateway>)> {
        let tables = lock(&self.tables);
        if !tables.crd || !tables.classes.synced || !tables.gateways.synced {
            return None;
        }
        let classes = tables.classes.items.values().cloned().collect();
        let gateways = tables
            .gateways
            .items
            .values()
            .filter(|g| {
                tables
                    .classes
                    .items
                    .contains_key(&(String::new(), g.class.clone()))
            })
            .cloned()
            .collect();
        Some((classes, gateways))
    }

    /// One pass: the status of each mimic class and of each Gateway of one that is not accepted.
    /// Returns when to run again after a failure.
    pub async fn pass(&self, now: &str) -> Option<Instant> {
        let mut state = self.state.lock().await;
        let (classes, gateways) = self.picture()?;
        let mut failed = false;
        for class in &classes {
            if let Plan::Write(body) = status::class(class, now)
                && let Err(error) = self
                    .cluster
                    .apply_status(StatusTarget::Class { name: &class.name }, &body)
                    .await
            {
                failed = true;
                tracing::debug!(class = class.name, error = %error, "gateway status not applied");
            }
        }
        let mut states = BTreeMap::new();
        for gw in &gateways {
            let Some(class) = classes.iter().find(|c| c.name == gw.class) else {
                continue;
            };
            let acceptance = accept::gateway(gw, class, &self.settings);
            if acceptance.is_accepted() {
                continue;
            }
            if let Plan::Write(body) = status::gateway(gw, &acceptance, &Observed::default(), now) {
                let target = StatusTarget::Gateway {
                    namespace: &gw.namespace,
                    name: &gw.name,
                };
                if let Err(error) = self.cluster.apply_status(target, &body).await {
                    failed = true;
                    tracing::debug!(
                        gateway = format!("{}/{}", gw.namespace, gw.name),
                        error = %error,
                        "gateway status not applied"
                    );
                }
            }
            self.metrics.count("invalid");
            states.insert((gw.namespace.clone(), gw.name.clone()), "not_accepted");
        }
        state.states = states;
        for kind in STATES {
            self.metrics
                .set(kind, state.states.values().filter(|s| **s == kind).count());
        }
        failed.then(|| Instant::now() + RETRY)
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
