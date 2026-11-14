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
    /// The `HTTPRoute`s; empty when the Gateway API CRDs are not installed.
    pub routes: Vec<Route>,
    /// `(namespace, name)` of the Deployments that exist.
    pub deployments: Vec<(String, String)>,
}

/// The API group of `HTTPRoute` and `ReferenceGrant`.
pub const GATEWAY_GROUP: &str = "gateway.networking.k8s.io";

/// `spec.parentRefs[]`. `group` and `kind` are as written (`kind` defaults to `Gateway` and
/// `group` to the Gateway API group, in the converter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRef {
    pub group: String,
    pub kind: String,
    /// `None`: the route's namespace.
    pub namespace: Option<String>,
    pub name: String,
    pub section_name: Option<String>,
    pub port: Option<u16>,
}

/// A `headers` or `queryParams` match; `kind` is `Exact` when the object does not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameMatch {
    pub kind: String,
    pub name: String,
    pub value: String,
}

/// `matches[].path`: `kind` `PathPrefix` and `value` `/` when the object does not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteMatch {
    pub path: Option<PathMatch>,
    pub headers: Vec<NameMatch>,
    pub query_params: Vec<NameMatch>,
    pub method: Option<String>,
}

/// A `backendRefs[]` entry; `group` is `""` and `kind` `Service` when the object does not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRef {
    pub group: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    pub port: Option<u16>,
    pub weight: Option<u32>,
    /// The entry has a `filters` list (not supported).
    pub has_filters: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderModifier {
    pub set: Vec<(String, String)>,
    pub add: Vec<(String, String)>,
    pub remove: Vec<String>,
}

/// `RequestMirror`: the backend, and the share of requests (`percent` or `fraction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirror {
    pub group: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    pub port: Option<u16>,
    pub percent: Option<u32>,
    /// `fraction`: numerator and denominator (`None`: 100).
    pub fraction: Option<(u32, Option<u32>)>,
}

/// A rule filter. `kind` is the `type`; only the member of that type is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFilter {
    pub kind: String,
    pub request_header_modifier: Option<HeaderModifier>,
    pub response_header_modifier: Option<HeaderModifier>,
    pub request_mirror: Option<Mirror>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteRule {
    pub name: Option<String>,
    pub matches: Vec<RouteMatch>,
    pub backend_refs: Vec<BackendRef>,
    pub filters: Vec<RouteFilter>,
    /// `timeouts.request` and `timeouts.backendRequest`, as written.
    pub request_timeout: Option<String>,
    pub backend_request_timeout: Option<String>,
    /// The rule has a `retry`.
    pub has_retry: bool,
}

/// A Gateway API `HTTPRoute`, without labels, annotations or status (RTE-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub generation: i64,
    /// `metadata.creationTimestamp`, seconds since the epoch.
    pub created: i64,
    pub parents: Vec<ParentRef>,
    pub rules: Vec<RouteRule>,
    /// Paths (`rules[0].sessionPersistence`) of the fields of `spec` the converter does not
    /// know, in document order; the field values are not kept.
    pub unknown_fields: Vec<String>,
    /// The `spec` was over 64 KiB: `rules` and `unknown_fields` are empty.
    pub oversize: bool,
}
