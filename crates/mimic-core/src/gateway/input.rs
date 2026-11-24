//! What the controller reads of a `Gateway` and its `GatewayClass` (spec §7.10 "What is read").
//! There is no field for labels, annotation values (but the replicas one) or `managedFields`:
//! they cannot reach a status message or a generated object (GW-02).

use serde_json::Value;

/// A Gateway API `Gateway` of a mimic class.
#[derive(Debug, Clone, PartialEq)]
pub struct Gateway {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub generation: i64,
    /// `spec.gatewayClassName`.
    pub class: String,
    /// `spec.listeners`, in order.
    pub listeners: Vec<Listener>,
    /// `spec.addresses` is not empty.
    pub has_addresses: bool,
    /// `spec.tls` is present.
    pub has_tls: bool,
    /// `spec.allowedListeners.namespaces.from`, as written.
    pub allowed_listeners_from: Option<String>,
    pub infrastructure: Infrastructure,
    /// Paths (`spec.defaultScope`) of the fields of `spec` outside `spec.listeners[]` the
    /// converter does not know; the field values are not kept.
    pub unknown_fields: Vec<String>,
    /// The `spec` was over 64 KiB: `listeners`, `infrastructure` and `unknown_fields` are empty.
    pub oversize: bool,
    /// The status already in the cluster.
    pub observed: ObservedStatus,
}

/// `spec.infrastructure`. Annotation values are dropped, but the replicas one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Infrastructure {
    /// `spec.infrastructure.labels` is not empty.
    pub has_labels: bool,
    /// Every key of `spec.infrastructure.annotations`, sorted.
    pub annotation_keys: Vec<String>,
    /// The value of `mimic.platform.io/replicas`, as written.
    pub replicas: Option<String>,
    pub has_parameters_ref: bool,
}

/// `spec.listeners[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub name: String,
    /// Kept for #457; never in a message, a log or a label.
    pub hostname: Option<String>,
    /// 0: unreadable.
    pub port: u16,
    pub protocol: String,
    /// `allowedRoutes.namespaces.from`; `None` is `Same`.
    pub allowed_from: Option<String>,
    /// `allowedRoutes.kinds`; `None` when absent.
    pub kinds: Option<Vec<RouteKind>>,
    /// Full paths (`spec.listeners[1].filters`) of the listener fields the converter does not
    /// know.
    pub unknown_fields: Vec<String>,
}

/// An `allowedRoutes.kinds[]` entry; the converter defaults `group` to the Gateway API group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteKind {
    pub group: String,
    pub kind: String,
}

/// A `GatewayClass` whose `controllerName` is mimic's.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayClass {
    pub name: String,
    pub generation: i64,
    pub has_parameters_ref: bool,
    /// `status.conditions` in the cluster.
    pub observed: Vec<Value>,
}

/// The `status` of a Gateway in the cluster: compared, never copied.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObservedStatus {
    pub conditions: Vec<Value>,
    pub listeners: Vec<Value>,
    pub addresses: Vec<Value>,
}
