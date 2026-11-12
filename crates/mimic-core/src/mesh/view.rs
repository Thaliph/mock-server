//! The scoped view of a deployment (spec §7.9, design §4.2): a pure function of a
//! [`Snapshot`], canonical JSON, an ETag, and the strict reader a sidecar uses.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::input::{EndpointSlice, Service, Snapshot, TargetKind, TargetPort};
use super::policy::{MAX_SLOW_START_SECS, Policy, Winners, consents, effective};
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
        // Plain structs and ordered maps: serialising them cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Reads a view, refusing what `build` never writes (MSH-12).
    pub fn from_json(text: &str, limits: &Limits) -> Result<View, String> {
        if text.len() > limits.max_view_bytes {
            return Err(format!("view over {} bytes", limits.max_view_bytes));
        }
        let view: View = serde_json::from_str(text).map_err(|e| short(&e.to_string()))?;
        if view.format != FORMAT {
            return Err(format!("unsupported view format {}", view.format));
        }
        if view.mode.parse::<Mode>().is_err() {
            return Err("unknown mode".into());
        }
        if view.services.len() > limits.max_services {
            return Err(format!("view over {} services", limits.max_services));
        }
        if view.endpoint_count() > limits.max_endpoints {
            return Err(format!("view over {} endpoints", limits.max_endpoints));
        }
        for (key, service) in &view.services {
            match key.split_once('/') {
                Some((ns, name)) if !ns.is_empty() && !name.is_empty() && !name.contains('/') => {}
                _ => return Err("a service key is not <namespace>/<name>".into()),
            }
            if service.policy.load_balancer.slow_start_seconds > MAX_SLOW_START_SECS {
                return Err(format!("slowStartSeconds over {MAX_SLOW_START_SECS}"));
            }
        }
        Ok(view)
    }

    /// Endpoints over all Services and ports.
    pub fn endpoint_count(&self) -> usize {
        self.services
            .values()
            .map(ServiceView::endpoint_count)
            .sum()
    }
}

impl ServiceView {
    fn endpoint_count(&self) -> usize {
        self.endpoints.values().map(Vec::len).sum()
    }
}

/// An error message cut to 200 bytes: a parser's message may quote the input.
fn short(message: &str) -> String {
    let mut end = message.len().min(200);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

/// The ETag of canonical bytes: `"1-<sha256 hex>"`.
pub fn etag(json: &str) -> String {
    let digest = Sha256::digest(json.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{FORMAT}-{hex}\"")
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
    /// Keys of the first Services left out (at most `MAX_SKIPPED_NAMES`), in order.
    pub skipped: Vec<String>,
}

/// What the Services and slices of a snapshot are looked up by.
struct Lookup<'a> {
    services: BTreeMap<(&'a str, &'a str), &'a Service>,
    slices: BTreeMap<(&'a str, &'a str), Vec<&'a EndpointSlice>>,
}

impl<'a> Lookup<'a> {
    fn new(snapshot: &'a Snapshot) -> Lookup<'a> {
        let mut slices: BTreeMap<(&str, &str), Vec<&EndpointSlice>> = BTreeMap::new();
        for slice in &snapshot.slices {
            slices
                .entry((slice.namespace.as_str(), slice.service.as_str()))
                .or_default()
                .push(slice);
        }
        for list in slices.values_mut() {
            list.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Lookup {
            services: snapshot
                .services
                .iter()
                .map(|s| ((s.namespace.as_str(), s.name.as_str()), s))
                .collect(),
            slices,
        }
    }
}

fn service_view(lookup: &Lookup<'_>, service: &Service, policy: Policy) -> ServiceView {
    let ips: BTreeSet<IpAddr> = service
        .cluster_ips
        .iter()
        .filter_map(|ip| ip.parse().ok())
        .collect();
    let slices = lookup
        .slices
        .get(&(service.namespace.as_str(), service.name.as_str()))
        .map_or(&[][..], Vec::as_slice);
    let generations = slices
        .iter()
        .map(|s| {
            (
                format!("endpointslice/{}/{}", s.namespace, s.name),
                s.generation,
            )
        })
        .collect();
    let mut ports = BTreeMap::new();
    let mut endpoints = BTreeMap::new();
    for port in &service.ports {
        ports.entry(port.port).or_insert_with(|| PortView {
            name: port.name.clone().filter(|n| !n.is_empty()),
            target_port: port.target_port.clone(),
        });
        let name = port.name.as_deref().unwrap_or("");
        let mut ready = Vec::new();
        let mut terminating = Vec::new();
        for slice in slices {
            for sp in slice
                .ports
                .iter()
                .filter(|p| p.name.as_deref().unwrap_or("") == name)
            {
                for e in &slice.endpoints {
                    let Ok(ip) = e.ip.parse::<IpAddr>() else {
                        continue;
                    };
                    let view = |ready| EndpointView {
                        ip,
                        port: sp.port,
                        ready,
                        zone: e.zone.clone().filter(|z| !z.is_empty()),
                    };
                    if e.ready {
                        ready.push(view(true));
                    } else if e.serving && e.terminating {
                        terminating.push(view(false));
                    }
                }
            }
        }
        let mut chosen = if ready.is_empty() { terminating } else { ready };
        chosen.sort();
        chosen.dedup_by_key(|e| (e.ip, e.port));
        endpoints.insert(port.port, chosen);
    }
    ServiceView {
        endpoints,
        generations,
        ips: ips.into_iter().collect(),
        policy,
        ports,
    }
}

/// The view of deployment `deployment` of `namespace` (spec §7.9).
pub fn build(
    snapshot: &Snapshot,
    namespace: &str,
    deployment: &str,
    mode: Mode,
    limits: &Limits,
) -> Built {
    let winners = Winners::new(&snapshot.configs);
    let own = winners.get(TargetKind::Deployment, namespace, deployment);
    let mut view = View {
        cluster: snapshot.cluster.clone(),
        format: FORMAT,
        generations: BTreeMap::new(),
        mesh: own.and_then(|c| c.mesh_enabled).unwrap_or(true),
        mode: mode.as_str().to_string(),
        services: BTreeMap::new(),
    };
    if let Some(c) = own {
        view.generations.insert(
            format!("mimicconfig/{}/{}", c.namespace, c.name),
            c.generation,
        );
    }
    if !view.mesh {
        view.generations.clear();
        let json = view.to_json();
        return Built {
            etag: etag(&json),
            view,
            json,
            truncated: 0,
            skipped: Vec::new(),
        };
    }
    let lookup = Lookup::new(snapshot);
    // Priority order: the Services of the namespace by name, then the declared destinations
    // that exist and consent, by key.
    let mut candidates: Vec<&Service> = lookup
        .services
        .range((namespace, "")..)
        .take_while(|((ns, _), _)| *ns == namespace)
        .map(|(_, s)| *s)
        .collect();
    let declared: BTreeSet<(&str, &str)> = own
        .and_then(|c| c.destinations.as_ref())
        .into_iter()
        .flatten()
        .filter(|d| d.namespace != namespace)
        .filter(|d| consents(snapshot.grants.as_deref(), namespace, &d.namespace, &d.name))
        .map(|d| (d.namespace.as_str(), d.name.as_str()))
        .collect();
    candidates.extend(
        declared
            .iter()
            .filter_map(|key| lookup.services.get(key).copied()),
    );

    let empty = view.to_json().len();
    let (mut bytes, mut endpoints) = (empty, 0);
    for (taken, service) in candidates.iter().enumerate() {
        let policy = effective(winners.get(TargetKind::Service, &service.namespace, &service.name));
        let mut sv = service_view(&lookup, service, policy);
        let key = service_key(&service.namespace, &service.name);
        if let Some(c) = winners.get(TargetKind::Service, &service.namespace, &service.name) {
            sv.generations.insert(
                format!("mimicconfig/{}/{}", c.namespace, c.name),
                c.generation,
            );
        }
        let entry = serde_json::to_string(&key).unwrap_or_default().len()
            + 1
            + serde_json::to_string(&sv).unwrap_or_default().len()
            + usize::from(taken > 0);
        let count = sv.endpoint_count();
        if taken >= limits.max_services
            || endpoints + count > limits.max_endpoints
            || bytes + entry > limits.max_view_bytes
        {
            let json = view.to_json();
            return Built {
                etag: etag(&json),
                view,
                json,
                truncated: candidates.len() - taken,
                skipped: Vec::new(),
            };
        }
        bytes += entry;
        endpoints += count;
        view.services.insert(key, sv);
    }
    let json = view.to_json();
    Built {
        etag: etag(&json),
        view,
        json,
        truncated: 0,
        skipped: Vec::new(),
    }
}
