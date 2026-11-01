//! The local report (spec §6): endpoints keyed by direction, port, method and path template,
//! with status counters, a latency histogram and the hosts seen. Merging is pure,
//! commutative and associative; recording an exchange merges a one-exchange report.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::histogram::Histogram;
use super::observed::{Observation, Observed, state_bound};
use super::redact::{MARKER, Redactor, value_rule};
use super::sample::{MAX_SAMPLES, Sample, Samples};
use super::template::{Seg, is_param, render, split_path};

/// Hosts kept per endpoint.
pub const MAX_HOSTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Inbound,
    Outbound,
}

/// Longest path recorded, in bytes.
pub const MAX_PATH: usize = 2048;
/// Most segments of a recorded path.
pub const MAX_SEGMENTS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `MIMIC_DISCOVERY_MAX_LITERALS`.
    pub max_literals: usize,
    /// `MIMIC_DISCOVERY_MAX_ENDPOINTS`.
    pub max_endpoints: usize,
    /// `MIMIC_DISCOVERY_SAMPLE_BUDGET`, in bytes.
    pub sample_budget: usize,
}

/// One relayed exchange, as the recorder sees it.
#[derive(Debug, Clone)]
pub struct Exchange<'a> {
    pub direction: Direction,
    pub port: u16,
    pub method: &'a str,
    /// Path without query.
    pub path: &'a str,
    pub status: u16,
    /// Time to the response head, in milliseconds.
    pub head_ms: u64,
    pub host: Option<&'a str>,
    /// `content-type: application/grpc*`: the path is never templated.
    pub grpc: bool,
    /// Query, headers and bodies (content-decoded).
    pub observation: Observation<'a>,
}

/// Why an exchange was not recorded, or what was kept of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Recorded {
        new_endpoint: bool,
        sample: Sampled,
    },
    /// Path over 2048 bytes or 32 segments, or a status outside 100..=599.
    Invalid,
    /// `404` or `405` on an endpoint that is not known.
    NotFound,
    /// A new endpoint past `max_endpoints`.
    EndpointLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampled {
    /// The exchange is a sample candidate.
    Admitted,
    /// The sample budget is full: counters and schemas changed, the sample was not admitted.
    Budget,
    /// No sample: a `404` or `405`.
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub statuses: BTreeMap<u16, u64>,
    pub latency: Histogram,
    pub hosts: BTreeSet<String>,
    observed: Observed,
    samples: Samples,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Node {
    literals: BTreeMap<String, Node>,
    param: Option<Box<Node>>,
    absorbing: bool,
    endpoints: BTreeMap<String, Stats>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Scope {
    root: Node,
    /// gRPC paths, never templated: path, then method.
    raw: BTreeMap<String, BTreeMap<String, Stats>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    version: u32,
    pod: Option<String>,
    exchanges: u64,
    scopes: BTreeMap<Direction, BTreeMap<u16, Scope>>,
}

/// An endpoint of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint<'a> {
    pub direction: Direction,
    pub port: u16,
    pub method: &'a str,
    pub template: String,
    pub stats: &'a Stats,
}

impl Stats {
    /// Counters, latency and host of one exchange.
    fn of(exchange: &Exchange<'_>) -> Self {
        let mut s = Stats::default();
        *s.statuses.entry(exchange.status).or_default() += 1;
        s.latency.observe(exchange.head_ms);
        // An authority has no userinfo (spec §4.1), but it is redacted like any other value.
        s.hosts.extend(exchange.host.map(|h| {
            if value_rule(h).is_some() {
                MARKER.to_string()
            } else {
                h.to_string()
            }
        }));
        s
    }

    pub fn samples(&self) -> &Samples {
        &self.samples
    }

    /// Schemas and volatility of the query, headers and bodies.
    pub fn observed(&self) -> &Observed {
        &self.observed
    }

    fn merge(&mut self, other: Stats) {
        for (status, n) in other.statuses {
            *self.statuses.entry(status).or_default() += n;
        }
        self.latency.merge(&other.latency);
        self.hosts.extend(other.hosts);
        while self.hosts.len() > MAX_HOSTS {
            self.hosts.pop_last();
        }
        self.observed = std::mem::take(&mut self.observed).merge(other.observed);
        self.samples = std::mem::take(&mut self.samples).merge(other.samples);
    }
}

fn merge_methods(into: &mut BTreeMap<String, Stats>, from: BTreeMap<String, Stats>) {
    for (method, stats) in from {
        into.entry(method).or_default().merge(stats);
    }
}

impl Node {
    /// Union of two subtrees. Past `max_literals` literal children a node absorbs: its
    /// literals and its parameter child become one parameter, for good. The result does not
    /// depend on the order of the merges.
    fn merge(&mut self, other: Node, max_literals: usize) {
        self.absorbing |= other.absorbing;
        merge_methods(&mut self.endpoints, other.endpoints);
        if let Some(param) = other.param {
            self.param
                .get_or_insert_with(Default::default)
                .merge(*param, max_literals);
        }
        for (literal, child) in other.literals {
            self.literals
                .entry(literal)
                .or_default()
                .merge(child, max_literals);
        }
        if self.absorbing || self.literals.len() > max_literals {
            self.absorbing = true;
            let literals = std::mem::take(&mut self.literals);
            if !literals.is_empty() {
                let param = self.param.get_or_insert_with(Default::default);
                for (_, child) in literals {
                    param.merge(child, max_literals);
                }
            }
        }
    }

    /// The node a path leads to, as the template tree would route it.
    fn find(&self, segments: &[&str]) -> Option<&Node> {
        let Some((first, rest)) = segments.split_first() else {
            return Some(self);
        };
        let next = if is_param(first) {
            self.param.as_deref()
        } else if let Some(child) = self.literals.get(*first) {
            Some(child)
        } else if self.absorbing {
            self.param.as_deref()
        } else {
            None
        };
        next?.find(rest)
    }

    fn for_each_stats<'a>(&'a self, f: &mut impl FnMut(&'a Stats)) {
        self.endpoints.values().for_each(&mut *f);
        for child in self.literals.values().chain(self.param.as_deref()) {
            child.for_each_stats(f);
        }
    }

    fn collect<'a>(&'a self, prefix: &mut Vec<Seg>, out: &mut Vec<(String, &'a str, &'a Stats)>) {
        let template = render(prefix);
        for (method, stats) in &self.endpoints {
            out.push((
                if prefix.is_empty() {
                    "/".into()
                } else {
                    template.clone()
                },
                method,
                stats,
            ));
        }
        for (literal, child) in &self.literals {
            prefix.push(Seg::Literal(literal.clone()));
            child.collect(prefix, out);
            prefix.pop();
        }
        if let Some(child) = &self.param {
            prefix.push(Seg::Param);
            child.collect(prefix, out);
            prefix.pop();
        }
    }
}

impl Scope {
    fn merge(&mut self, other: Scope, max_literals: usize) {
        self.root.merge(other.root, max_literals);
        for (path, methods) in other.raw {
            merge_methods(self.raw.entry(path).or_default(), methods);
        }
    }
}

impl Scope {
    fn for_each_stats<'a>(&'a self, f: &mut impl FnMut(&'a Stats)) {
        self.root.for_each_stats(f);
        for methods in self.raw.values() {
            methods.values().for_each(&mut *f);
        }
    }
}

impl Report {
    pub fn new(pod: Option<String>) -> Self {
        Report {
            version: 1,
            pod,
            exchanges: 0,
            scopes: BTreeMap::new(),
        }
    }

    pub fn exchanges(&self) -> u64 {
        self.exchanges
    }

    fn for_each_stats<'a>(&'a self, mut f: impl FnMut(&'a Stats)) {
        for scope in self.scopes.values().flat_map(BTreeMap::values) {
            scope.for_each_stats(&mut f);
        }
    }

    /// The stats of the endpoint an exchange belongs to, when it is already known.
    fn find(&self, exchange: &Exchange<'_>) -> Option<&Stats> {
        let scope = self.scopes.get(&exchange.direction)?.get(&exchange.port)?;
        if exchange.grpc {
            return scope.raw.get(exchange.path)?.get(exchange.method);
        }
        scope
            .root
            .find(&split_path(exchange.path))?
            .endpoints
            .get(exchange.method)
    }

    /// Endpoints held, whatever their direction and port.
    fn endpoint_count(&self) -> usize {
        let mut n = 0;
        self.for_each_stats(|_| n += 1);
        n
    }

    /// Bytes of the samples held (what `MIMIC_DISCOVERY_SAMPLE_BUDGET` limits).
    pub fn sample_bytes(&self) -> usize {
        let mut n = 0;
        self.for_each_stats(|s| n += s.samples.bytes());
        n
    }

    /// Upper bound of the serialised report without its samples, from the limits alone.
    pub fn state_bound(limits: &Limits) -> usize {
        // Path, hosts, statuses (100..=599), histogram buckets, the framing of the samples
        // and of the scope, and the schemas.
        let per_endpoint = MAX_PATH
            + MAX_SEGMENTS * 16
            + MAX_HOSTS * 300
            + 500 * 16
            + 70 * 16
            + MAX_SAMPLES * 256
            + 256
            + state_bound();
        limits.max_endpoints.saturating_mul(per_endpoint) + 1024
    }

    /// Records one exchange: the same as merging a one-exchange report, once the bounds
    /// (spec §6) allow it.
    pub fn record(
        &mut self,
        limits: &Limits,
        redactor: &Redactor,
        exchange: &Exchange<'_>,
    ) -> Outcome {
        let segments = split_path(exchange.path);
        if exchange.path.len() > MAX_PATH
            || segments.len() > MAX_SEGMENTS
            || !(100..=599).contains(&exchange.status)
        {
            return Outcome::Invalid;
        }
        let known = self.find(exchange);
        let counted_only = matches!(exchange.status, 404 | 405);
        if known.is_none() {
            if counted_only {
                return Outcome::NotFound;
            }
            if self.endpoint_count() >= limits.max_endpoints {
                return Outcome::EndpointLimit;
            }
        }
        let new_endpoint = known.is_none();
        let mut stats = Stats::of(exchange);
        let mut sampled = Sampled::None;
        if !counted_only {
            let (request, response) = stats
                .observed
                .record_parsed(redactor, &exchange.observation);
            let (hash, sample) = Sample::new(redactor, exchange, request, response);
            let present = known.is_some_and(|k| k.samples.contains(hash));
            if present || self.sample_bytes() + sample.bytes() <= limits.sample_budget {
                stats.samples = Samples::single(hash, sample);
                sampled = Sampled::Admitted;
            } else {
                sampled = Sampled::Budget;
            }
        }
        let methods = BTreeMap::from([(exchange.method.to_string(), stats)]);
        let mut scope = Scope::default();
        if exchange.grpc {
            scope.raw.insert(exchange.path.to_string(), methods);
        } else {
            let mut node = Node {
                endpoints: methods,
                ..Node::default()
            };
            // The root node stands for "/", whose first segment is its child.
            for segment in segments.into_iter().rev() {
                let mut parent = Node::default();
                if is_param(segment) {
                    parent.param = Some(Box::new(node));
                } else {
                    parent.literals.insert(segment.to_string(), node);
                }
                node = parent;
            }
            scope.root = node;
        }
        let mut one = Report::new(self.pod.clone());
        one.exchanges = 1;
        one.scopes
            .entry(exchange.direction)
            .or_default()
            .insert(exchange.port, scope);
        self.merge(limits, one);
        Outcome::Recorded {
            new_endpoint,
            sample: sampled,
        }
    }

    /// Merges `other` into `self`.
    pub fn merge(&mut self, limits: &Limits, other: Report) {
        if self.pod != other.pod {
            self.pod = None;
        }
        self.exchanges += other.exchanges;
        for (direction, ports) in other.scopes {
            let mine = self.scopes.entry(direction).or_default();
            for (port, scope) in ports {
                mine.entry(port)
                    .or_default()
                    .merge(scope, limits.max_literals);
            }
        }
    }

    /// Endpoints sorted by direction, port, template, method.
    pub fn endpoints(&self) -> Vec<Endpoint<'_>> {
        let mut out = Vec::new();
        for (direction, ports) in &self.scopes {
            for (port, scope) in ports {
                let mut found = Vec::new();
                scope.root.collect(&mut Vec::new(), &mut found);
                for (path, methods) in &scope.raw {
                    found.extend(methods.iter().map(|(m, s)| (path.clone(), m.as_str(), s)));
                }
                found.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
                out.extend(found.into_iter().map(|(template, method, stats)| Endpoint {
                    direction: *direction,
                    port: *port,
                    method,
                    template,
                    stats,
                }));
            }
        }
        out
    }

    /// Canonical JSON: sorted keys, `"version": 1`.
    pub fn to_json(&self) -> String {
        // Maps with sorted keys and plain data cannot fail to serialise.
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        let report: Report =
            serde_json::from_str(json).map_err(|e| format!("invalid report: {e}"))?;
        if report.version != 1 {
            return Err(format!("unsupported report version {}", report.version));
        }
        Ok(report)
    }
}

/// Pure merge: commutative and associative.
pub fn merge(mut a: Report, b: Report, limits: &Limits) -> Report {
    a.merge(limits, b);
    a
}
