//! The cluster side of the reconciler: Job and NetworkPolicy calls, the status apply, and the
//! watch that feeds [`Tests`].

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
    PostParams, PropagationPolicy,
};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use serde_json::Value;
use tokio::sync::OnceCell;

use super::{Cluster, ClusterFuture, JobState, Kind, Reconciler, Tests, parse};

pub type Object = DynamicObject;

const MANAGER: &str = "mimic-controller";

fn resource(kind: Kind) -> ApiResource {
    match kind {
        Kind::Job => ApiResource::from_gvk(&GroupVersionKind::gvk("batch", "v1", "Job")),
        Kind::NetworkPolicy => ApiResource::from_gvk(&GroupVersionKind::gvk(
            "networking.k8s.io",
            "v1",
            "NetworkPolicy",
        )),
    }
}

fn load_test_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(
        "mimic.platform.io",
        "v1alpha1",
        "MimicLoadTest",
    ))
}

/// The real cluster. `client` is set once the cluster client is available.
pub struct KubeCluster {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeCluster {
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeCluster {
        KubeCluster { client }
    }

    fn api(&self, namespace: &str, resource: &ApiResource) -> Result<Api<DynamicObject>, String> {
        let client = self
            .client
            .get()
            .cloned()
            .ok_or_else(|| "no Kubernetes client yet".to_string())?;
        Ok(Api::namespaced_with(client, namespace, resource))
    }
}

fn not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(status) if status.code == 404)
}

impl Cluster for KubeCluster {
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        kind: Kind,
        object: &'a Value,
    ) -> ClusterFuture<'a, bool> {
        Box::pin(async move {
            let api = self.api(namespace, &resource(kind))?;
            let object: DynamicObject =
                serde_json::from_value(object.clone()).map_err(|e| e.to_string())?;
            match api.create(&PostParams::default(), &object).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(status)) if status.code == 409 => Ok(false),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn delete<'a>(
        &'a self,
        namespace: &'a str,
        kind: Kind,
        name: &'a str,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let api = self.api(namespace, &resource(kind))?;
            let params = DeleteParams {
                propagation_policy: Some(PropagationPolicy::Background),
                ..DeleteParams::default()
            };
            match api.delete(name, &params).await {
                Ok(_) => Ok(()),
                Err(e) if not_found(&e) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    fn job_state<'a>(&'a self, namespace: &'a str, name: &'a str) -> ClusterFuture<'a, JobState> {
        Box::pin(async move {
            let client = self
                .client
                .get()
                .cloned()
                .ok_or_else(|| "no Kubernetes client yet".to_string())?;
            let job = match Api::<Job>::namespaced(client, namespace).get(name).await {
                Ok(job) => job,
                Err(e) if not_found(&e) => return Ok(JobState::Gone),
                Err(e) => return Err(e.to_string()),
            };
            let condition = |kind: &str| {
                job.status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|c| c.iter().any(|c| c.type_ == kind && c.status == "True"))
            };
            Ok(if condition("Failed") {
                JobState::Failed
            } else if condition("Complete") {
                JobState::Complete
            } else {
                JobState::Running
            })
        })
    }

    fn apply_status<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        status: &'a Value,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let api = self.api(namespace, &load_test_resource())?;
            let body = serde_json::json!({
                "apiVersion": "mimic.platform.io/v1alpha1",
                "kind": "MimicLoadTest",
                "metadata": {"name": name},
                "status": status,
            });
            // Forced: the status is the controller's alone.
            let params = PatchParams::apply(MANAGER).force();
            api.patch_status(name, &params, &Patch::Apply(&body))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// Keeps `tests` in line with the `MimicLoadTest`s of the cluster, for ever; an object that
/// disappears ends its run.
pub async fn watch(client: kube::Client, reconciler: Arc<Reconciler>) {
    let api = Api::<DynamicObject>::all_with(client, &load_test_resource());
    let mut events =
        std::pin::pin!(watcher::watcher(api, watcher::Config::default()).default_backoff());
    let tests: &Tests = &reconciler.tests;
    let mut listing = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => listing.clear(),
            Ok(Event::InitApply(object)) => listing.extend(parse(&object)),
            Ok(Event::InitDone) => {
                reconciler.resync(std::mem::take(&mut listing)).await;
                tracing::info!("MimicLoadTest cache synced");
            }
            Ok(Event::Apply(object)) => match parse(&object) {
                Some(test) => tests.apply(test),
                None => tracing::warn!("MimicLoadTest ignored: its spec does not parse"),
            },
            Ok(Event::Delete(object)) => {
                if let Some(test) = parse(&object) {
                    tests.remove(&test.namespace, &test.name);
                    reconciler.deleted(&test).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "MimicLoadTest watch error"),
        }
    }
}
