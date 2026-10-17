//! Adaptateur Kubernetes : surveille les `MimicWorkload` et publie leur statut (spec §5.5.1).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use kube::ResourceExt;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};

use crate::crd::{MimicWorkload, MimicWorkloadStatus, StatusCondition};
use crate::state::Ctl;

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
                    let s = ctl.summary(w);
                    Some(MimicWorkloadStatus {
                        snapshot: w.desired.snapshot.clone(),
                        pods: s.pods,
                        aligned: s.aligned,
                        observed_generation: w.generation,
                        conditions: vec![StatusCondition {
                            type_: "Aligned".into(),
                            status: if s.ready { "True" } else { "False" }.into(),
                            reason: s.reason,
                        }],
                    })
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
