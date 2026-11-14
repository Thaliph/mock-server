//! The scoped view of a deployment (spec §7.9, design §4.2): a pure function of a
//! [`Snapshot`], canonical JSON, an ETag, and the strict reader a sidecar uses.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::input::{EndpointSlice, Route, Service, Snapshot, TargetKind, TargetPort};
use super::policy::{MAX_RETRIES, MAX_SLOW_START_SECS, Policy, Winners, consents, effective};
use super::route::{Compiled, ModifierView, RouteView, RuleView, compile, refused_header};
use crate::Mode;

/// The view format this code writes and reads.
pub const FORMAT: u32 = 1;

/// How many skipped Services `Built::skipped` names.
pub const MAX_SKIPPED_NAMES: usize = 16;

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
    /// Oldest route first; absent when none applies (spec §7.9 "Routes").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteView>,
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
        for key in view
            .generations
            .keys()
            .chain(view.services.values().flat_map(|s| s.generations.keys()))
        {
            if !generation_key(key) {
                return Err("a generations key is not <kind>/<namespace>/<name>@<uid>".into());
            }
        }
        for (key, service) in &view.services {
            match key.split_once('/') {
                Some((ns, name)) if !ns.is_empty() && !name.is_empty() && !name.contains('/') => {}
                _ => return Err("a service key is not <namespace>/<name>".into()),
            }
            if service.policy.load_balancer.slow_start_seconds > MAX_SLOW_START_SECS {
                return Err(format!("slowStartSeconds over {MAX_SLOW_START_SECS}"));
            }
            if service.policy.retries > MAX_RETRIES {
                return Err(format!("retries over {MAX_RETRIES}"));
            }
            if !service.policy.retry_budget.in_bounds() {
                return Err("retryBudget out of bounds".into());
            }
        }
        check_routes(&view)?;
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

/// The HTTPRoute CRD maxima (spec §7.9 "`View::from_json`").
const MAX_RULES: usize = 16;
const MAX_MATCHES: usize = 64;
const MAX_ITEMS: usize = 16;
const MAX_WEIGHT: u32 = 1_000_000;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_PATH_BYTES: usize = 1024;
const METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// A lower-case HTTP token.
fn header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"!#$%&'*+-.^_`|~".contains(&b)
        })
}

fn header_value(value: &str) -> bool {
    value.len() <= MAX_VALUE_BYTES && !value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

/// Refuses in `routes` what `build` never writes (RTE-12).
fn check_routes(view: &View) -> Result<(), String> {
    // A Service of the view with the port, as a backend or mirror names it.
    let held = |service: &str, port: u16| {
        view.services
            .get(service)
            .is_some_and(|s| s.ports.contains_key(&port))
    };
    for service in view.services.values() {
        for route in &service.routes {
            if route.name.split('/').filter(|p| !p.is_empty()).count() != 2 {
                return Err("a route name is not <namespace>/<name>".into());
            }
            if let Some(ports) = &route.ports
                && (ports.is_empty() || ports.iter().any(|p| !service.ports.contains_key(p)))
            {
                return Err("a route port is not a port of its service".into());
            }
            if route.rules.len() > MAX_RULES {
                return Err(format!("route over {MAX_RULES} rules"));
            }
            for rule in &route.rules {
                check_rule(rule, &held)?;
            }
        }
    }
    Ok(())
}

fn check_rule(rule: &RuleView, held: &dyn Fn(&str, u16) -> bool) -> Result<(), String> {
    if rule.matches.len() > MAX_MATCHES {
        return Err(format!("rule over {MAX_MATCHES} matches"));
    }
    for m in &rule.matches {
        if m.headers.len() > MAX_ITEMS || m.query_params.len() > MAX_ITEMS {
            return Err(format!("match over {MAX_ITEMS} headers or query params"));
        }
        for h in &m.headers {
            if !header_name(&h.name) {
                return Err("a header match name is not a lower-case token".into());
            }
        }
        for nv in m.headers.iter().chain(&m.query_params) {
            if !header_value(&nv.value) {
                return Err("a match value is not valid".into());
            }
        }
        if let Some(path) = &m.path
            && (!matches!(path.kind.as_str(), "Exact" | "PathPrefix")
                || !path.value.starts_with('/')
                || path.value.len() > MAX_PATH_BYTES
                || path.value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)))
        {
            return Err("a path match is not valid".into());
        }
        if m.method.as_deref().is_some_and(|m| !METHODS.contains(&m)) {
            return Err("a method is not one of the CRD".into());
        }
    }
    if rule.backends.len() > MAX_ITEMS {
        return Err(format!("rule over {MAX_ITEMS} backends"));
    }
    for b in &rule.backends {
        let shape_ok = match (b.invalid, &b.service, b.port) {
            (Some(true), None, None) => true,
            (None, Some(service), Some(port)) => held(service, port),
            _ => false,
        };
        if !shape_ok {
            return Err("a backend is invalid or names a service or port not held".into());
        }
        if b.weight > MAX_WEIGHT {
            return Err(format!("weight over {MAX_WEIGHT}"));
        }
    }
    if let Some(filters) = &rule.filters {
        if filters.mirrors.len() > MAX_ITEMS {
            return Err(format!("rule over {MAX_ITEMS} mirrors"));
        }
        for m in &filters.mirrors {
            if !held(&m.service, m.port) {
                return Err("a mirror names a service or port not held".into());
            }
            if m.denominator == 0 || m.numerator > m.denominator {
                return Err("a mirror fraction is out of bounds".into());
            }
        }
        for modifier in [&filters.request_headers, &filters.response_headers]
            .into_iter()
            .flatten()
        {
            check_modifier(modifier)?;
        }
    }
    Ok(())
}

fn check_modifier(m: &ModifierView) -> Result<(), String> {
    if m.set.len() > MAX_ITEMS || m.add.len() > MAX_ITEMS || m.remove.len() > MAX_ITEMS {
        return Err(format!("modifier over {MAX_ITEMS} items"));
    }
    for name in m
        .set
        .iter()
        .chain(&m.add)
        .map(|nv| &nv.name)
        .chain(&m.remove)
    {
        if !header_name(name) || refused_header(name) {
            return Err("a modifier name is not allowed".into());
        }
    }
    if m.set
        .iter()
        .chain(&m.add)
        .any(|nv| !header_value(&nv.value))
    {
        return Err("a modifier value is not valid".into());
    }
    Ok(())
}

/// `<kind>/<namespace>/<name>@<uid>`: the identity of an object including its UID, so that an
/// object deleted and created again is another key (spec §7.9).
fn generation_key(key: &str) -> bool {
    let Some((id, uid)) = key.rsplit_once('@') else {
        return false;
    };
    let parts: Vec<&str> = id.split('/').collect();
    key.len() <= 600 && !uid.is_empty() && parts.len() == 3 && parts.iter().all(|p| !p.is_empty())
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
    /// `<namespace>/<name>` of the first routes left out of a Service because they name a
    /// Service the view does not hold (at most `MAX_SKIPPED_NAMES`), in order.
    pub skipped_routes: Vec<String>,
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
                format!("endpointslice/{}/{}@{}", s.namespace, s.name, s.uid),
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
        routes: Vec::new(),
    }
}

/// A programmed route and its compilation.
struct RouteEntry<'a> {
    route: &'a Route,
    compiled: Compiled,
}

impl RouteEntry<'_> {
    fn key(&self) -> String {
        format!("{}/{}", self.route.namespace, self.route.name)
    }
}

/// The routes of `service` for a caller of `caller` (GAMMA): the consumer routes of the caller's
/// namespace when there are any, else the producer routes, oldest first (`entries` is sorted).
fn scoped_routes<'a, 'b>(
    entries: &'a [RouteEntry<'b>],
    service: &Service,
    caller: &str,
) -> Vec<&'a RouteEntry<'b>> {
    let key = service_key(&service.namespace, &service.name);
    let pick = |ns: &str| -> Vec<&'a RouteEntry<'b>> {
        entries
            .iter()
            .filter(|e| e.route.namespace == ns && e.compiled.attachments.contains_key(&key))
            .collect()
    };
    if caller != service.namespace {
        let consumer = pick(caller);
        if !consumer.is_empty() {
            return consumer;
        }
    }
    pick(&service.namespace)
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
            format!("mimicconfig/{}/{}@{}", c.namespace, c.name, c.uid),
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
            skipped_routes: Vec::new(),
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

    let mut entries: Vec<RouteEntry> = snapshot
        .routes
        .iter()
        .map(|route| RouteEntry {
            route,
            compiled: compile(route, &snapshot.services, snapshot.grants.as_deref()),
        })
        .filter(|e| e.compiled.programmed)
        .collect();
    entries.sort_by_key(|e| (e.route.created, e.key()));

    let empty = view.to_json().len();
    let mut fit = Fit {
        bytes: empty,
        endpoints: 0,
        skipped: Vec::new(),
        truncated: 0,
    };
    let mut in_scope = BTreeSet::new();
    for service in &candidates {
        let key = service_key(&service.namespace, &service.name);
        in_scope.insert(key.clone());
        let mut routes = scoped_routes(&entries, service, namespace);
        routes.retain(|e| e.compiled.view_for(&key).is_some());
        fit.add(&mut view, limits, &winners, &lookup, service, &routes);
    }

    // The closure (depth 1): what the routes of the view name and the view does not hold yet,
    // in key order, after every Service of the scope.
    let by_name: BTreeMap<String, &RouteEntry> = entries.iter().map(|e| (e.key(), e)).collect();
    let wanted: BTreeSet<String> = view
        .services
        .values()
        .flat_map(|s| &s.routes)
        .filter_map(|r| by_name.get(&r.name))
        .flat_map(|e| e.compiled.targets())
        .map(|(service, _)| service)
        .filter(|key| !view.services.contains_key(key) && !in_scope.contains(key))
        .collect();
    for key in &wanted {
        let service = key
            .split_once('/')
            .and_then(|k| lookup.services.get(&k).copied());
        if let Some(service) = service {
            fit.add(&mut view, limits, &winners, &lookup, service, &[]);
        }
    }

    // A route that names a Service the view does not hold is left out of its Service.
    let mut skipped_routes = BTreeSet::new();
    let held: BTreeSet<String> = view.services.keys().cloned().collect();
    for service in view.services.values_mut() {
        service.routes.retain(|r| {
            let Some(e) = by_name.get(&r.name) else {
                return false;
            };
            let complete = e.compiled.targets().iter().all(|(s, _)| held.contains(s));
            if !complete {
                skipped_routes.insert(r.name.clone());
                service.generations.remove(&format!(
                    "httproute/{}/{}@{}",
                    e.route.namespace, e.route.name, e.route.uid
                ));
            }
            complete
        });
    }
    let json = view.to_json();
    Built {
        etag: etag(&json),
        view,
        json,
        truncated: fit.truncated,
        skipped: fit.skipped,
        skipped_routes: skipped_routes.into_iter().take(MAX_SKIPPED_NAMES).collect(),
    }
}

/// The running totals of the bounds while Services are added in priority order.
struct Fit {
    bytes: usize,
    endpoints: usize,
    skipped: Vec<String>,
    truncated: usize,
}

impl Fit {
    /// Adds `service` with its `routes` when it fits what is left of the bounds, else counts it
    /// as left out and tries the next one.
    fn add(
        &mut self,
        view: &mut View,
        limits: &Limits,
        winners: &Winners<'_>,
        lookup: &Lookup<'_>,
        service: &Service,
        routes: &[&RouteEntry<'_>],
    ) {
        let key = service_key(&service.namespace, &service.name);
        // Once `max_services` is reached nothing else fits: no need to build the rest.
        let fitted = if view.services.len() >= limits.max_services {
            None
        } else {
            let config = winners.get(TargetKind::Service, &service.namespace, &service.name);
            let mut sv = service_view(lookup, service, effective(config));
            if let Some(c) = config {
                sv.generations.insert(
                    format!("mimicconfig/{}/{}@{}", c.namespace, c.name, c.uid),
                    c.generation,
                );
            }
            for e in routes {
                if let Some(rv) = e.compiled.view_for(&key) {
                    sv.routes.push(rv);
                    sv.generations.insert(
                        format!(
                            "httproute/{}/{}@{}",
                            e.route.namespace, e.route.name, e.route.uid
                        ),
                        e.route.generation,
                    );
                }
            }
            let entry = serde_json::to_string(&key).unwrap_or_default().len()
                + 1
                + serde_json::to_string(&sv).unwrap_or_default().len()
                + usize::from(!view.services.is_empty());
            let count = sv.endpoint_count();
            (self.endpoints + count <= limits.max_endpoints
                && self.bytes + entry <= limits.max_view_bytes)
                .then_some((sv, entry, count))
        };
        match fitted {
            Some((sv, entry, count)) => {
                self.bytes += entry;
                self.endpoints += count;
                view.services.insert(key, sv);
            }
            None => {
                self.truncated += 1;
                if self.skipped.len() < MAX_SKIPPED_NAMES {
                    self.skipped.push(key);
                }
            }
        }
    }
}
