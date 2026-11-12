//! The Map tab (spec §7.5, *Map tab*): the graph of which deployment calls which, built from
//! the stored outbound counters and the watched Services, laid out in layers and rendered as
//! SVG. Pure and deterministic: the same inputs in any order give the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use crate::mesh::input::{Snapshot, TargetKind};
use crate::mesh::policy::{Winners, consents};

mod layout;
mod render;

/// What one deployment sent to one authority: the stored counters, summed over its statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub authority: String,
    pub exchanges: u64,
    /// The exchanges whose recorded status is 5xx.
    pub errors: u64,
}

/// A deployment with stored data and what it called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub namespace: String,
    pub deployment: String,
    pub calls: Vec<Call>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_deployments: usize,
    pub max_others: usize,
    pub max_edges: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_deployments: 1000,
            max_others: 200,
            max_edges: 2000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deployment,
    Service,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub kind: Kind,
    pub namespace: String,
    pub name: String,
    pub label: String,
    pub layer: usize,
    pub x: i64,
    pub y: i64,
    pub href: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub exchanges: u64,
    pub errors: u64,
    pub error_permille: u64,
    pub declared: bool,
    pub href: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    pub nodes: usize,
    pub edges: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Map {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub dropped: Dropped,
    pub width: i64,
    pub height: i64,
}

/// The height and width of a node's box in the layout.
pub const NODE_H: i64 = 36;
pub const NODE_W: i64 = 200;

/// An authority is cut here before it is used anywhere (spec §7.5).
const MAX_AUTHORITY: usize = 255;

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Deployment => "deployment",
            Kind::Service => "service",
            Kind::External => "external",
        }
    }
}

/// `text` cut to at most `max` characters, on a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// `text` shown in at most `max` characters: the rest is replaced by `…`.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().nth(max).is_none() {
        return text.to_string();
    }
    format!("{}…", truncate(text, max - 1))
}

/// A DNS-1123 label.
fn label_ok(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// What an authority resolves to.
enum Target {
    /// A Service of the cluster, as `(namespace, name)`.
    Service(String, String),
    External,
}

/// What resolution reads: the watched Services and the deployments with data.
struct Context<'a> {
    services: BTreeSet<(&'a str, &'a str)>,
    ips: BTreeMap<String, (&'a str, &'a str)>,
    deployments: BTreeSet<(&'a str, &'a str)>,
}

impl Context<'_> {
    /// A name that is a Service or a deployment with data.
    fn known(&self, namespace: &str, name: &str) -> bool {
        self.services.contains(&(namespace, name)) || self.deployments.contains(&(namespace, name))
    }
}

/// The host of an authority, lower-cased, without port, brackets or final dot.
fn host_of(authority: &str) -> String {
    let lower = authority.to_ascii_lowercase();
    let host = match lower
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        Some((inner, _)) => inner,
        None if lower.matches(':').count() == 1 => lower.split_once(':').map_or(&*lower, |h| h.0),
        None => &lower,
    };
    host.strip_suffix('.').unwrap_or(host).to_string()
}

/// The rules of spec §7.5, *Map tab*.
fn resolve(authority: &str, caller: &str, ctx: &Context<'_>) -> Target {
    let host = host_of(authority);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ctx.ips.get(&ip.to_string()) {
            Some((ns, name)) => Target::Service((*ns).to_string(), (*name).to_string()),
            None => Target::External,
        };
    }
    let labels: Vec<&str> = host.split('.').collect();
    if !labels.iter().all(|l| label_ok(l)) {
        return Target::External;
    }
    let service = |ns: &str, name: &str| Target::Service(ns.to_string(), name.to_string());
    match labels.as_slice() {
        [name] if ctx.known(caller, name) => service(caller, name),
        [name, ns] if ctx.known(ns, name) => service(ns, name),
        [name, ns, "svc", ..] => service(ns, name),
        _ => Target::External,
    }
}

/// Whether the caller's winning `MimicConfig` declares the Service and its namespace consents
/// (spec §7.9); true when nothing is checked.
fn declared(
    mesh: Option<(&Snapshot, &Winners<'_>)>,
    caller: (&str, &str),
    service: (&str, &str),
) -> bool {
    let Some((snapshot, winners)) = mesh else {
        return true;
    };
    if caller.0 == service.0 {
        return true;
    }
    winners
        .get(TargetKind::Deployment, caller.0, caller.1)
        .and_then(|c| c.destinations.as_ref())
        .is_some_and(|dests| {
            dests
                .iter()
                .any(|d| d.namespace == service.0 && d.name == service.1)
        })
        && consents(snapshot.grants.as_deref(), caller.0, service.0, service.1)
}

/// `(exchanges, errors)` per authority.
type Sums<'a> = BTreeMap<&'a str, (u64, u64)>;

struct Weight {
    exchanges: u64,
    errors: u64,
    declared: bool,
}

fn href_deployment(namespace: &str, deployment: &str) -> String {
    format!(
        "/ui/workloads/{}/{}",
        render::percent(namespace),
        render::percent(deployment)
    )
}

fn href_endpoints(namespace: &str, deployment: &str) -> String {
    format!(
        "/ui/endpoints?deployment={}%2F{}",
        render::percent(namespace),
        render::percent(deployment)
    )
}

/// The map of `sources`; `mesh` is the hub's snapshot once its lists are complete (`None`: the
/// mesh is off or not listed yet, nothing is flagged and only names resolve).
pub fn build(sources: &[Source], mesh: Option<&Snapshot>, limits: &Limits) -> Map {
    // What each deployment sent to each authority, summed over duplicated sources and calls.
    let mut called: BTreeMap<(&str, &str), Sums> = BTreeMap::new();
    for source in sources {
        let calls = called
            .entry((&source.namespace, &source.deployment))
            .or_default();
        for call in &source.calls {
            let sum = calls
                .entry(truncate(&call.authority, MAX_AUTHORITY))
                .or_default();
            sum.0 = sum.0.saturating_add(call.exchanges);
            sum.1 = sum.1.saturating_add(call.errors);
        }
    }
    let services: Vec<_> = mesh.map_or(&[][..], |m| &m.services[..]).iter().collect();
    let mut ctx = Context {
        services: services
            .iter()
            .map(|s| (s.namespace.as_str(), s.name.as_str()))
            .collect(),
        ips: BTreeMap::new(),
        deployments: called.keys().copied().collect(),
    };
    for s in &services {
        for ip in s
            .cluster_ips
            .iter()
            .filter_map(|ip| ip.parse::<IpAddr>().ok())
        {
            ctx.ips
                .entry(ip.to_string())
                .or_insert((&s.namespace, &s.name));
        }
    }
    let winners = mesh.map(|m| Winners::new(&m.configs));
    let checked = mesh.zip(winners.as_ref());
    let kept: BTreeSet<(&str, &str)> = called
        .keys()
        .copied()
        .take(limits.max_deployments)
        .collect();
    let mut dropped = Dropped {
        nodes: called.len() - kept.len(),
        edges: 0,
    };

    // The edges between node ids, before the bounds.
    let mut edges: BTreeMap<(String, String), Weight> = BTreeMap::new();
    let mut leaves: BTreeMap<String, Node> = BTreeMap::new();
    for (&(ns, dep), calls) in &called {
        let from = format!("dep:{ns}/{dep}");
        for (&authority, &(exchanges, errors)) in calls {
            let (to, flagged) = match resolve(authority, ns, &ctx) {
                Target::Service(sns, name) => {
                    let id = if ctx.deployments.contains(&(sns.as_str(), name.as_str())) {
                        format!("dep:{sns}/{name}")
                    } else {
                        let node = Node {
                            id: format!("svc:{sns}/{name}"),
                            kind: Kind::Service,
                            label: format!("{sns}/{name}"),
                            namespace: sns.clone(),
                            name: name.clone(),
                            layer: 0,
                            x: 0,
                            y: 0,
                            href: None,
                        };
                        let id = node.id.clone();
                        leaves.entry(id.clone()).or_insert(node);
                        id
                    };
                    (id, declared(checked, (ns, dep), (&sns, &name)))
                }
                Target::External => {
                    let node = Node {
                        id: format!("ext:{authority}"),
                        kind: Kind::External,
                        namespace: String::new(),
                        name: authority.to_string(),
                        label: authority.to_string(),
                        layer: 0,
                        x: 0,
                        y: 0,
                        href: None,
                    };
                    let id = node.id.clone();
                    leaves.entry(id.clone()).or_insert(node);
                    (id, true)
                }
            };
            if to == from {
                continue;
            }
            let weight = edges.entry((from.clone(), to)).or_insert(Weight {
                exchanges: 0,
                errors: 0,
                declared: flagged,
            });
            weight.exchanges = weight.exchanges.saturating_add(exchanges);
            weight.errors = weight.errors.saturating_add(errors);
        }
    }

    // Bounds: an edge with a dropped deployment at an end goes first, then the lightest other
    // nodes with their edges, then the lightest edges.
    let is_kept = |id: &str| match id.strip_prefix("dep:").and_then(|r| r.split_once('/')) {
        Some((ns, dep)) => kept.contains(&(ns, dep)),
        None => true,
    };
    let before = edges.len();
    edges.retain(|(from, to), _| is_kept(from) && is_kept(to));
    dropped.edges += before - edges.len();
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    for ((_, to), w) in &edges {
        if leaves.contains_key(to) {
            let total = totals.entry(to).or_default();
            *total = total.saturating_add(w.exchanges);
        }
    }
    let mut ranked: Vec<(&str, u64)> = totals.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let others: BTreeSet<String> = ranked
        .iter()
        .take(limits.max_others)
        .map(|(id, _)| (*id).to_string())
        .collect();
    dropped.nodes += ranked.len() - others.len();
    let before = edges.len();
    edges.retain(|(_, to), _| !leaves.contains_key(to) || others.contains(to));
    dropped.edges += before - edges.len();
    let mut ordered: Vec<((String, String), Weight)> = edges.into_iter().collect();
    ordered.sort_by(|a, b| b.1.exchanges.cmp(&a.1.exchanges).then(a.0.cmp(&b.0)));
    if ordered.len() > limits.max_edges {
        dropped.edges += ordered.len() - limits.max_edges;
        ordered.truncate(limits.max_edges);
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let referenced: BTreeSet<&str> = ordered.iter().map(|((_, to), _)| to.as_str()).collect();
    dropped.nodes += others
        .iter()
        .filter(|id| !referenced.contains(id.as_str()))
        .count();

    let mut nodes: BTreeMap<String, Node> = kept
        .iter()
        .map(|&(ns, dep)| {
            let id = format!("dep:{ns}/{dep}");
            let node = Node {
                id: id.clone(),
                kind: Kind::Deployment,
                namespace: ns.to_string(),
                name: dep.to_string(),
                label: format!("{ns}/{dep}"),
                layer: 0,
                x: 0,
                y: 0,
                href: Some(href_deployment(ns, dep)),
            };
            (id, node)
        })
        .collect();
    for id in &referenced {
        if let Some(node) = leaves.remove(*id) {
            nodes.insert(node.id.clone(), node);
        }
    }
    let edges: Vec<Edge> = ordered
        .into_iter()
        .map(|((from, to), w)| {
            let href = match from.strip_prefix("dep:").and_then(|r| r.split_once('/')) {
                Some((ns, dep)) => href_endpoints(ns, dep),
                None => String::new(),
            };
            Edge {
                error_permille: u64::try_from(
                    u128::from(w.errors) * 1000 / u128::from(w.exchanges.max(1)),
                )
                .unwrap_or(u64::MAX),
                from,
                to,
                exchanges: w.exchanges,
                errors: w.errors,
                declared: w.declared,
                href,
            }
        })
        .collect();
    let mut map = Map {
        nodes: nodes.into_values().collect(),
        edges,
        dropped,
        width: 0,
        height: 0,
    };
    layout::place(&mut map);
    map
}

impl Map {
    /// The JSON answer of `GET /v1/map`: sorted keys, so the same map is the same bytes.
    pub fn to_json(&self) -> String {
        render::json(self)
    }

    /// The SVG of the map.
    pub fn to_svg(&self) -> String {
        render::svg(self)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_prop;
