//! The mimic Gateway, pure part (spec §7.10; design `docs/design/mimic-mesh.md` §5, §15): the
//! plain input structs the controller fills from a `Gateway` and its `GatewayClass`, their
//! acceptance, the generated objects and the status.

use std::collections::BTreeSet;

use serde_json::Value;

pub mod accept;
pub mod input;
pub mod objects;
pub mod status;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests_accept;
#[cfg(test)]
mod tests_names;
#[cfg(test)]
mod tests_objects;
#[cfg(test)]
mod tests_prop;
#[cfg(test)]
mod tests_status;

/// The `controllerName` of the mimic GatewayClasses.
pub const CONTROLLER_NAME: &str = "mimic.platform.io/gateway-controller";
/// The gateway's admin port (`/readyz`, `/healthz`, `/metrics`): never a listener port.
pub const ADMIN_PORT: u16 = 15100;
/// The only `spec.infrastructure.annotations` key a Gateway may set.
pub const REPLICAS_ANNOTATION: &str = "mimic.platform.io/replicas";
/// Replicas of a Gateway without the annotation (capped by `Settings::max_replicas`).
pub const DEFAULT_REPLICAS: u32 = 2;
/// Longest Gateway, GatewayClass and generated object name.
pub const MAX_NAME: usize = 63;
/// Most listeners of a Gateway (the CRD's `maxItems`; `MIMIC_GATEWAY_PORTS` takes at most 64).
pub const MAX_LISTENERS: usize = 64;

/// The label naming the Gateway of a generated object; the selector of its pods.
pub const GATEWAY_LABEL: &str = "mimic.platform.io/gateway";
/// The label that, with [`GATEWAY_LABEL`], marks an object as mimic's.
pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
/// The value of [`MANAGED_BY`] on the generated objects.
pub const MANAGER: &str = "mimic";
/// Name of the admin container port.
pub const ADMIN_PORT_NAME: &str = "mimic-admin";
/// User and group of the gateway pod.
pub const UID: i64 = 65532;
/// Most `status.addresses` entries written.
pub const MAX_ADDRESSES: usize = 16;
/// `apiVersion` of `Gateway` and `GatewayClass`.
pub const GATEWAY_API_VERSION: &str = "gateway.networking.k8s.io/v1";

/// `type` of every gateway Service (`MIMIC_GATEWAY_SERVICE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServiceType {
    ClusterIP,
    NodePort,
    LoadBalancer,
}

impl ServiceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceType::ClusterIP => "ClusterIP",
            ServiceType::NodePort => "NodePort",
            ServiceType::LoadBalancer => "LoadBalancer",
        }
    }
}

/// An object with `labels` is managed by mimic for the Gateway `gateway` (§7.10 "Apply,
/// ownership and deletion"): it carries both labels of the generated objects.
pub fn managed_for(labels: &Value, gateway: &str) -> bool {
    labels[MANAGED_BY] == MANAGER && labels[GATEWAY_LABEL] == gateway
}

/// The controller configuration the pure functions read (§10.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `MIMIC_GATEWAY_NAMESPACES`.
    pub namespaces: BTreeSet<String>,
    /// `MIMIC_GATEWAY_MAX_REPLICAS`, 1 to 100.
    pub max_replicas: u32,
    /// `MIMIC_GATEWAY_SERVICE_TYPE`.
    pub service_type: ServiceType,
    /// `MIMIC_INJECT_SIDECAR_IMAGE`.
    pub image: String,
    /// `MIMIC_INJECT_SIDECAR_LOG`.
    pub log: String,
    /// `MIMIC_GATEWAY_RESOURCES`, a JSON `resources` object.
    pub resources: Value,
}
