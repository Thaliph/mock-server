//! The cluster side of the gateway reconciler: the server-side applies of the status of
//! Gateways and GatewayClasses.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams};
use serde_json::Value;
use tokio::sync::OnceCell;

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

/// The object whose status is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTarget<'a> {
    Gateway { namespace: &'a str, name: &'a str },
    Class { name: &'a str },
}

pub type ClusterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

/// What the reconciler asks of the cluster.
pub trait Cluster: Send + Sync {
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

fn gateway_api(kind: &str) -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(
        "gateway.networking.k8s.io",
        "v1",
        kind,
    ))
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
}

impl Cluster for KubeCluster {
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
                        &gateway_api("Gateway"),
                    ),
                    name,
                ),
                StatusTarget::Class { name } => (
                    Api::<DynamicObject>::all_with(client, &gateway_api("GatewayClass")),
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
