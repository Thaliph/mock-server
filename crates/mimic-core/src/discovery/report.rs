//! The local report (spec §6): endpoints keyed by direction, port, method and path template,
//! with status counters, a latency histogram and the hosts seen. Merging is pure,
//! commutative and associative; recording an exchange merges a one-exchange report.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::histogram::Histogram;
use super::template::{Seg, is_param, render, split_path};

/// Hosts kept per endpoint.
pub const MAX_HOSTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `MIMIC_DISCOVERY_MAX_LITERALS`.
    pub max_literals: usize,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub statuses: BTreeMap<u16, u64>,
    pub latency: Histogram,
    pub hosts: BTreeSet<String>,
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
    fn of(exchange: &Exchange<'_>) -> Self {
        let mut s = Stats::default();
        *s.statuses.entry(exchange.status).or_default() += 1;
        s.latency.observe(exchange.head_ms);
        s.hosts.extend(exchange.host.map(str::to_string));
        s
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

    /// Records one exchange: the same as merging a one-exchange report.
    pub fn record(&mut self, limits: &Limits, exchange: &Exchange<'_>) {
        let stats = Stats::of(exchange);
        let mut scope = Scope::default();
        if exchange.grpc {
            let methods = BTreeMap::from([(exchange.method.to_string(), stats)]);
            scope.raw.insert(exchange.path.to_string(), methods);
        } else {
            let mut node = Node {
                endpoints: BTreeMap::from([(exchange.method.to_string(), stats)]),
                ..Node::default()
            };
            // The root node stands for "/", whose first segment is its child.
            for segment in split_path(exchange.path).into_iter().rev() {
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
