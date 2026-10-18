//! Adaptateur Kubernetes : surveille les `MimicWorkload` et publie leur statut (spec §5.5.1).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};

use crate::crd::{MimicWorkload, MimicWorkloadStatus};
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
pub async fn watch(ctl: Arc<Ctl>, client: kube::Client) {
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
                .await;
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
