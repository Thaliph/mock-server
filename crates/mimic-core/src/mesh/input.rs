//! What the view is built from: plain structs, no Kubernetes types. The controller (#441)
//! converts its watched objects into them. They have no field for a label, an annotation or a
//! Secret value, so none can reach a view (MSH-16).

use serde::{Deserialize, Serialize};

/// The API group of `MimicConfig`, as a `ReferenceGrant` `from` names it.
pub const MIMIC_GROUP: &str = "mimic.platform.io";

/// A Service port's `targetPort`: a number or a container port name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetPort {
    Number(u16),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePort {
    pub port: u16,
    /// `None` for the unnamed port of a single-port Service.
    pub name: Option<String>,
    pub target_port: TargetPort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub namespace: String,
    pub name: String,
    /// `spec.clusterIPs`; `None` (headless) or an unparsable entry is dropped.
    pub cluster_ips: Vec<String>,
    pub ports: Vec<ServicePort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicePort {
    /// The Service port name this port belongs to (`None`: unnamed).
    pub name: Option<String>,
    /// The port number on the endpoints.
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub ip: String,
    /// `conditions.ready`, a missing value read as `true` by the adapter.
    pub ready: bool,
    pub serving: bool,
    pub terminating: bool,
    pub zone: Option<String>,
}

/// An `EndpointSlice`, with the Service it belongs to (label `kubernetes.io/service-name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSlice {
    pub namespace: String,
    pub name: String,
    pub service: String,
    /// `metadata.uid`: a recreated object has another one, so its generation restarts unseen.
    pub uid: String,
    pub generation: i64,
    pub ports: Vec<SlicePort>,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    Deployment,
    Service,
}

/// A `targetRef`: always in the namespace of its `MimicConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    pub name: String,
}

/// `spec.destinations[]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Destination {
    pub namespace: String,
    pub name: String,
}

/// A `MimicConfig`; `None` is a field the object does not set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub namespace: String,
    pub name: String,
    /// `metadata.creationTimestamp`, seconds since the epoch.
    pub created: i64,
    /// `metadata.uid`, as for the slices.
    pub uid: String,
    pub generation: i64,
    pub targets: Vec<Target>,
    pub destinations: Option<Vec<Destination>>,
    pub mesh_enabled: Option<bool>,
    /// `spec.loadBalancer.slowStart` in seconds (a present `loadBalancer` without it is `None`
    /// here: it sets nothing).
    pub slow_start: Option<u32>,
    /// `spec.retries` (a Service target only).
    pub retries: Option<u32>,
    /// `spec.retryBudget`; `None` when the object has none (a present one without fields is
    /// `Some` of all `None`).
    pub retry_budget: Option<RetryBudgetSpec>,
}

/// `spec.retryBudget`; `None` is a field the object does not set. A value the adapter cannot
/// read (a `window` that is not `<n>s`) arrives out of bounds, so that `validate` refuses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryBudgetSpec {
    pub percent: Option<u32>,
    pub min_per_second: Option<u32>,
    /// `window` in seconds.
    pub window_seconds: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantFrom {
    pub group: String,
    pub kind: String,
    pub namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTo {
    pub group: String,
    pub kind: String,
    pub name: Option<String>,
}

/// A Gateway API `ReferenceGrant`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceGrant {
    pub namespace: String,
    pub from: Vec<GrantFrom>,
    pub to: Vec<GrantTo>,
}

/// Everything the controller has watched, at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// `MIMIC_CLUSTER_NAME`.
    pub cluster: String,
    pub services: Vec<Service>,
    pub slices: Vec<EndpointSlice>,
    pub configs: Vec<Config>,
    /// `None`: the `ReferenceGrant` CRD is not installed (no cross-namespace consent).
    pub grants: Option<Vec<ReferenceGrant>>,
    /// `(namespace, name)` of the Deployments that exist.
    pub deployments: Vec<(String, String)>,
}
