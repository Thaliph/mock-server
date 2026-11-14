//! HTTPRoute compilation (spec §7.9 "Routes", #447): a [`Route`] becomes the route of the view
//! and the conditions of its parent entries. Pure: the Services and grants come in as arguments.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::input::{ReferenceGrant, Route, Service};
use super::policy::Condition;

/// Field order is alphabetical in every struct below: the JSON is canonical (sorted keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathView {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MatchView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<NameValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathView>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_params: Vec<NameValue>,
}

/// A valid backend (`service` and `port`) or an invalid one (`invalid`, nothing else).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModifierView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<NameValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<NameValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorView {
    pub denominator: u32,
    pub numerator: u32,
    pub port: u16,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FiltersView {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<MirrorView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<ModifierView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<ModifierView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimeoutsView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_request_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleView {
    pub backends: Vec<BackendView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<FiltersView>,
    pub matches: Vec<MatchView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<TimeoutsView>,
}

/// A route as one Service's view holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteView {
    /// `<namespace>/<name>` of the route.
    pub name: String,
    /// Absent: every port of the Service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
    pub rules: Vec<RuleView>,
}

/// The conditions of one `status.parents[]` entry of mimic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentStatus {
    /// Index in `Route::parents`.
    pub index: usize,
    pub accepted: Condition,
    pub resolved_refs: Condition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// `false`: the route is in no view (`Accepted: False`, `UnsupportedValue`).
    pub programmed: bool,
    /// One per Service parent that exists, in `parentRefs` order.
    pub parents: Vec<ParentStatus>,
    /// `<namespace>/<name>` of each Service the route is attached to, with the ports it applies
    /// to (`None`: every port). Empty when not programmed.
    pub attachments: BTreeMap<String, Option<Vec<u16>>>,
    name: String,
    rules: Vec<RuleView>,
}

impl Compiled {
    /// The route as the view of Service `service` holds it.
    pub fn view_for(&self, service: &str) -> Option<RouteView> {
        let _ = (&self.name, &self.rules, service);
        todo!()
    }

    /// Every `(service, port)` the rules name as valid backend or mirror.
    pub fn targets(&self) -> Vec<(String, u16)> {
        todo!()
    }
}

/// Compiles `route` against the Services of the cluster and its `ReferenceGrant`s (`None`: the
/// CRD is not installed).
pub fn compile(route: &Route, services: &[Service], grants: Option<&[ReferenceGrant]>) -> Compiled {
    let _ = (route, services, grants);
    todo!()
}
