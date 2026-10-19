//! Adaptateur Kubernetes : surveille les `MimicWorkload` et publie leur statut (spec §5.5.1),
//! tient le bail de la réplique active et sa marque (spec §5.5.4).

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use k8s_openapi::api::coordination::v1::{Lease as KubeLeaseObject, LeaseSpec};
use k8s_openapi::api::core::v1::{Namespace, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::api::{ObjectMeta, PostParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use mimic_core::lease::{Lease, Record};

use crate::crd::{MimicWorkload, MimicWorkloadStatus};
use crate::leader::{Coordination, DELETION_COST, LEADER_LABEL};
use crate::reconcile::status_of;
use crate::state::Ctl;

/// Première lecture des labels de tous les namespaces, avant de servir les sidecars : sans
/// elle, un redémarrage laisserait une fenêtre sans garde-fou production (spec §5.5.1).
pub async fn load_namespaces(ctl: &Ctl, client: &kube::Client) -> Result<(), String> {
    let api: Api<Namespace> = Api::all(client.clone());
    let list = api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!("liste des namespaces (garde-fou production): {e}"))?;
    for ns in list.items {
        ctl.set_namespace_labels(&ns.name_any(), ns.labels()).await;
    }
    Ok(())
}

/// Suit les labels des namespaces : poser ou retirer `env=production` réconcilie les
/// ressources du namespace.
pub async fn watch_namespaces(ctl: Arc<Ctl>, client: kube::Client) {
    let api: Api<Namespace> = Api::all(client);
    let mut events = watcher::watcher(api, watcher::Config::default())
        .default_backoff()
        .boxed();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Apply(ns) | Event::InitApply(ns)) => {
                ctl.set_namespace_labels(&ns.name_any(), ns.labels()).await;
            }
            Ok(Event::Delete(ns)) => {
                ctl.set_namespace_labels(&ns.name_any(), &BTreeMap::new())
                    .await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "surveillance des namespaces"),
        }
    }
}

const STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// Applique chaque création, modification ou suppression de ressource à l'état du contrôleur.
/// `init` est signalé une fois la liste initiale appliquée.
pub async fn watch(
    ctl: Arc<Ctl>,
    client: kube::Client,
    mut init: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let api: Api<MimicWorkload> = Api::all(client);
    let mut events = watcher::watcher(api, watcher::Config::default())
        .default_backoff()
        .boxed();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Apply(mw) | Event::InitApply(mw)) => {
                let ns = mw.namespace().unwrap_or_default();
                let name = mw.name_any();
                ctl.apply_spec(&ns, &name, Some(mw.spec.clone()), mw.metadata.generation)
                    .await;
            }
            Ok(Event::Delete(mw)) => {
                let ns = mw.namespace().unwrap_or_default();
                ctl.apply_spec(&ns, &mw.name_any(), None, None).await;
            }
            Ok(Event::InitDone) => {
                if let Some(tx) = init.take() {
                    let _ = tx.send(());
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "surveillance des MimicWorkload"),
        }
    }
}

/// Publie périodiquement le statut des ressources dont l'alignement a changé.
pub async fn publish_status(ctl: Arc<Ctl>, client: kube::Client) {
    let mut last: BTreeMap<(String, String), MimicWorkloadStatus> = BTreeMap::new();
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let keys = ctl.workload_keys().await;
        // Les workloads disparus ne gardent pas leur dernier statut publié.
        last.retain(|k, _| keys.binary_search(k).is_ok());
        for (ns, name) in keys {
            // Lecture de l'état court : jamais derrière un calcul ou une I/O.
            let status = ctl
                .with_live(&ns, &name, |w| {
                    w.spec.as_ref()?;
                    Some(status_of(&w.desired, &ctl.summary(w), w.generation))
                })
                .await
                .ok()
                .flatten();
            let Some(status) = status else {
                continue;
            };
            let key = (ns.clone(), name.clone());
            if last.get(&key) == Some(&status) {
                continue;
            }
            let api: Api<MimicWorkload> = Api::namespaced(client.clone(), &ns);
            let patch = serde_json::json!({ "status": status });
            match api
                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
            {
                Ok(_) => {
                    last.insert(key, status);
                }
                Err(e) => {
                    tracing::warn!(error = %e, namespace = %ns, name = %name, "statut non publié")
                }
            }
        }
    }
}

/// Bail `coordination.k8s.io/v1` et marque de la réplique active sur les pods du contrôleur.
pub struct KubeLease {
    leases: Api<KubeLeaseObject>,
    pods: Api<Pod>,
    name: String,
    /// Détenteur lu en dernier : une écriture pour un autre détenteur est une prise.
    last_holder: std::sync::Mutex<Option<String>>,
}

impl KubeLease {
    pub fn new(client: kube::Client, namespace: &str, name: &str) -> Self {
        Self {
            leases: Api::namespaced(client.clone(), namespace),
            pods: Api::namespaced(client, namespace),
            name: name.into(),
            last_holder: std::sync::Mutex::new(None),
        }
    }

    fn last_holder(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.last_holder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Pose (ou retire) la marque de la réplique active sur un pod.
    async fn mark(&self, pod: &str, active: bool) -> Result<(), String> {
        let (label, value) = LEADER_LABEL;
        let (cost, weight) = DELETION_COST;
        let patch = serde_json::json!({"metadata": {
            "labels": { label: active.then_some(value) },
            "annotations": { cost: active.then_some(weight) },
        }});
        self.pods
            .patch(pod, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map(|_| ())
            .map_err(|e| format!("marque du pod {pod}: {e}"))
    }
}

impl Coordination for KubeLease {
    fn get(&self) -> BoxFuture<'_, Result<Option<Lease>, String>> {
        Box::pin(async move {
            let found = self
                .leases
                .get_opt(&self.name)
                .await
                .map_err(|e| format!("lecture du bail {}: {e}", self.name))?;
            let lease = found.map(|l| {
                let spec = l.spec.unwrap_or_default();
                Lease {
                    record: Record {
                        holder: spec.holder_identity.filter(|h| !h.is_empty()),
                        duration_ms: spec.lease_duration_seconds.unwrap_or(0).max(0) as u64 * 1_000,
                        transitions: spec.lease_transitions.unwrap_or(0).max(0) as u32,
                    },
                    version: l.metadata.resource_version.unwrap_or_default(),
                }
            });
            *self.last_holder() = lease.as_ref().and_then(|l| l.record.holder.clone());
            Ok(lease)
        })
    }

    fn write(&self, record: Record, expected: Option<String>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let now = MicroTime(k8s_openapi::jiff::Timestamp::now());
            let acquired = record.holder.is_some() && record.holder != *self.last_holder();
            let spec = LeaseSpec {
                holder_identity: record.holder.clone(),
                lease_duration_seconds: Some((record.duration_ms / 1_000).max(1) as i32),
                lease_transitions: Some(record.transitions as i32),
                renew_time: record.holder.is_some().then(|| now.clone()),
                acquire_time: acquired.then_some(now),
                ..LeaseSpec::default()
            };
            let result = match expected {
                None => self
                    .leases
                    .create(
                        &PostParams::default(),
                        &KubeLeaseObject {
                            metadata: ObjectMeta {
                                name: Some(self.name.clone()),
                                ..ObjectMeta::default()
                            },
                            spec: Some(spec),
                        },
                    )
                    .await
                    .map(|_| ()),
                // Patch conditionnel : la version donnée est une précondition (409 sinon).
                Some(version) => {
                    let mut body = serde_json::json!({
                        "metadata": {"resourceVersion": version},
                        "spec": spec,
                    });
                    // Un bail rendu n'a plus de détenteur : le champ est retiré.
                    if record.holder.is_none() {
                        body["spec"]["holderIdentity"] = serde_json::Value::Null;
                    }
                    self.leases
                        .patch(&self.name, &PatchParams::default(), &Patch::Merge(&body))
                        .await
                        .map(|_| ())
                }
            };
            result.map_err(|e| format!("écriture du bail {}: {e}", self.name))
        })
    }

    fn announce(&self, me: &str, active: bool) -> BoxFuture<'_, Result<(), String>> {
        let me = me.to_string();
        Box::pin(async move {
            if active {
                // Une ancienne active arrêtée sans avoir retiré sa marque la perd ici.
                let (label, value) = LEADER_LABEL;
                let marked = self
                    .pods
                    .list(&ListParams::default().labels(&format!("{label}={value}")))
                    .await
                    .map_err(|e| format!("pods marqués: {e}"))?;
                for p in marked.items {
                    let name = p.name_any();
                    if name != me {
                        self.mark(&name, false).await?;
                    }
                }
            }
            self.mark(&me, active).await
        })
    }
}
