//! The cluster side of the gateway reconciler: the server-side applies of the generated objects
//! and of the status of Gateways and GatewayClasses, the reads and deletes of the objects.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams,
    Preconditions, PropagationPolicy,
};
use mimic_core::gateway::input::ObjectKind;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::mesh::watch::gateway_api;

/// The field manager of every apply of the gateway reconciler.
pub const FIELD_MANAGER: &str = "mimic-gateway";

/// A failed request: the API status reason and code only, never a message or a body (they can
/// quote fields of the object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub reason: String,
    pub code: u16,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.reason, self.code)
    }
}

impl ApiError {
    /// Not an API status: no connection, no client yet, a body that does not parse.
    pub fn unavailable() -> ApiError {
        ApiError {
            reason: "Unavailable".into(),
            code: 0,
        }
    }
}

impl From<&kube::Error> for ApiError {
    fn from(error: &kube::Error) -> ApiError {
        match error {
            kube::Error::Api(status) => ApiError {
                reason: if status.reason.is_empty() {
                    "Unknown".into()
                } else {
                    status.reason.clone()
                },
                code: status.code,
            },
            _ => ApiError::unavailable(),
        }
    }
}

impl From<&kube::runtime::watcher::Error> for ApiError {
    fn from(error: &kube::runtime::watcher::Error) -> ApiError {
        use kube::runtime::watcher::Error;
        match error {
            Error::InitialListFailed(e) | Error::WatchStartFailed(e) | Error::WatchFailed(e) => {
                ApiError::from(e)
            }
            Error::WatchError(status) => ApiError::from(&kube::Error::Api(status.clone())),
            _ => ApiError::unavailable(),
        }
    }
}

/// The object whose status is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTarget<'a> {
    Gateway { namespace: &'a str, name: &'a str },
    Class { name: &'a str },
}

pub type ClusterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

/// What the reconciler asks of the cluster.
pub trait Cluster: Send + Sync {
    /// Server-side apply, [`FIELD_MANAGER`], force. Returns the object the API server answers.
    fn apply<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        body: &'a Value,
    ) -> ClusterFuture<'a, Value>;
    /// A 404 is `Ok(None)`.
    fn get<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
    ) -> ClusterFuture<'a, Option<Value>>;
    /// Background propagation, the UID as precondition. A 404 or a 409 (another object of
    /// that name now) is `Ok(())`.
    fn delete<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        uid: &'a str,
    ) -> ClusterFuture<'a, ()>;
    /// Server-side apply on the status subresource, [`FIELD_MANAGER`], force.
    fn apply_status<'a>(
        &'a self,
        target: StatusTarget<'a>,
        body: &'a Value,
    ) -> ClusterFuture<'a, ()>;
}

/// Forced: the class's controller is the only writer of a Gateway's status (§7.10).
pub fn apply_params() -> PatchParams {
    PatchParams::apply(FIELD_MANAGER).force()
}

fn object_api(kind: ObjectKind) -> ApiResource {
    let (group, version) = match kind {
        ObjectKind::Deployment => ("apps", "v1"),
        ObjectKind::Service => ("", "v1"),
        ObjectKind::PodDisruptionBudget => ("policy", "v1"),
    };
    ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind.as_str()))
}

fn json(object: DynamicObject) -> Result<Value, ApiError> {
    serde_json::to_value(object).map_err(|_| ApiError::unavailable())
}

/// The real cluster. `client` is set once the cluster client is available.
pub struct KubeCluster {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeCluster {
    /// Requests fail until `client` is set.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeCluster {
        KubeCluster { client }
    }

    fn client(&self) -> Result<kube::Client, ApiError> {
        self.client.get().cloned().ok_or_else(ApiError::unavailable)
    }

    fn objects(&self, kind: ObjectKind, namespace: &str) -> Result<Api<DynamicObject>, ApiError> {
        Ok(Api::namespaced_with(
            self.client()?,
            namespace,
            &object_api(kind),
        ))
    }
}

impl Cluster for KubeCluster {
    fn apply<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        body: &'a Value,
    ) -> ClusterFuture<'a, Value> {
        Box::pin(async move {
            let api = self.objects(kind, namespace)?;
            let answer = api
                .patch(name, &apply_params(), &Patch::Apply(body))
                .await
                .map_err(|e| ApiError::from(&e))?;
            json(answer)
        })
    }

    fn get<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
    ) -> ClusterFuture<'a, Option<Value>> {
        Box::pin(async move {
            let api = self.objects(kind, namespace)?;
            match api.get_opt(name).await {
                Ok(Some(object)) => json(object).map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(ApiError::from(&e)),
            }
        })
    }

    fn delete<'a>(
        &'a self,
        kind: ObjectKind,
        namespace: &'a str,
        name: &'a str,
        uid: &'a str,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let api = self.objects(kind, namespace)?;
            let params = DeleteParams {
                propagation_policy: Some(PropagationPolicy::Background),
                preconditions: Some(Preconditions {
                    uid: Some(uid.to_string()),
                    resource_version: None,
                }),
                ..DeleteParams::default()
            };
            match api.delete(name, &params).await {
                Ok(_) => Ok(()),
                Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => Ok(()),
                Err(e) => Err(ApiError::from(&e)),
            }
        })
    }

    fn apply_status<'a>(
        &'a self,
        target: StatusTarget<'a>,
        body: &'a Value,
    ) -> ClusterFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client()?;
            let (api, name) = match target {
                StatusTarget::Gateway { namespace, name } => (
                    Api::<DynamicObject>::namespaced_with(
                        client,
                        namespace,
                        &gateway_api("v1", "Gateway"),
                    ),
                    name,
                ),
                StatusTarget::Class { name } => (
                    Api::<DynamicObject>::all_with(client, &gateway_api("v1", "GatewayClass")),
                    name,
                ),
            };
            api.patch_status(name, &apply_params(), &Patch::Apply(body))
                .await
                .map(|_| ())
                .map_err(|e| ApiError::from(&e))
        })
    }
}
