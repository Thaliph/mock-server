//! The scoped view of a deployment (spec §7.9, design §4.2): a pure function of a
//! [`Snapshot`], canonical JSON, an ETag, and the strict reader a sidecar uses.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use super::input::{Snapshot, TargetPort};
use super::policy::Policy;
use crate::Mode;

/// The view format this code writes and reads.
pub const FORMAT: u32 = 1;

/// `MIMIC_MESH_MAX_*` (spec §10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_services: usize,
    pub max_endpoints: usize,
    pub max_view_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_services: 1024,
            max_endpoints: 10_000,
            max_view_bytes: 4 << 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PortView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub target_port: TargetPort,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointView {
    pub ip: IpAddr,
    pub port: u16,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
}

/// Field order is alphabetical: the JSON is canonical (sorted keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceView {
    pub endpoints: BTreeMap<u16, Vec<EndpointView>>,
    pub generations: BTreeMap<String, i64>,
    pub ips: Vec<IpAddr>,
    pub policy: Policy,
    pub ports: BTreeMap<u16, PortView>,
}

/// Field order is alphabetical: the JSON is canonical (sorted keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub cluster: String,
    pub format: u32,
    pub generations: BTreeMap<String, i64>,
    pub mesh: bool,
    pub mode: String,
    pub services: BTreeMap<String, ServiceView>,
}

impl View {
    /// The canonical JSON.
    pub fn to_json(&self) -> String {
        todo!()
    }

    /// Reads a view, refusing what `build` never writes (MSH-12).
    pub fn from_json(text: &str, limits: &Limits) -> Result<View, String> {
        let _ = (text, limits);
        todo!()
    }

    /// Endpoints over all Services and ports.
    pub fn endpoint_count(&self) -> usize {
        todo!()
    }
}

/// The ETag of canonical bytes: `"1-<sha256 hex>"`.
pub fn etag(json: &str) -> String {
    let _ = json;
    todo!()
}

/// The key of a Service in a view; the one place that could prefix a cluster (design §16).
pub fn service_key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

/// A built view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub view: View,
    pub json: String,
    pub etag: String,
    /// Services left out because of a bound.
    pub truncated: usize,
}

/// The view of deployment `deployment` of `namespace` (spec §7.9).
pub fn build(
    snapshot: &Snapshot,
    namespace: &str,
    deployment: &str,
    mode: Mode,
    limits: &Limits,
) -> Built {
    let _ = (snapshot, namespace, deployment, mode, limits);
    todo!()
}
