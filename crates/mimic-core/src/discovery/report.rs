//! The local report (spec §6): endpoints keyed by direction, port, method and path template,
//! with status counters, a latency histogram and the hosts seen. Merging is pure,
//! commutative and associative; recording an exchange merges a one-exchange report.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::check::Violation;
use super::histogram::Histogram;
use super::observed::{Observation, Observed, state_bound};
use super::redact::{self, MARKER, Redactor, value_rule};
use super::sample::{MAX_SAMPLES, Sample, Samples};
use super::template::{Seg, is_param, render, split_path};

/// Hosts kept per endpoint.
pub const MAX_HOSTS: usize = 16;
/// Longest host kept, in bytes (the codec cuts the authority there).
pub const MAX_HOST: usize = 1024;

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
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub statuses: BTreeMap<u16, u64>,
    pub latency: Histogram,
    pub hosts: BTreeSet<String>,
    observed: Observed,
    samples: Samples,
    /// The UTC day (days since 1970-01-01) of the last push that held the endpoint. Set by
    /// the controller (spec §7.3, stamping); a sidecar never serialises it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    literals: BTreeMap<String, Node>,
    param: Option<Box<Node>>,
    absorbing: bool,
    endpoints: BTreeMap<String, Stats>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    root: Node,
    /// gRPC paths, never templated: path, then method.
    raw: BTreeMap<String, BTreeMap<String, Stats>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    pub fn last_seen(&self) -> Option<u64> {
        self.last_seen
    }

    pub(super) fn set_last_seen(&mut self, last_seen: Option<u64>) {
        self.last_seen = last_seen;
    }

    pub(super) fn samples_mut(&mut self) -> &mut Samples {
        &mut self.samples
    }

    /// Exchanges counted on the endpoint.
    pub(super) fn total(&self) -> u64 {
        self.statuses.values().fold(0, |n, c| n.saturating_add(*c))
    }

    /// Statuses 100..=599, histogram, at most `MAX_HOSTS` hosts of at most `MAX_HOST` bytes,
    /// schemas and samples within their bounds.
    fn check(&self) -> Result<(), Violation> {
        if self.statuses.keys().any(|s| !(100..=599).contains(s)) {
            return Err(Violation::Status);
        }
        self.latency.check()?;
        if self.hosts.len() > MAX_HOSTS {
            return Err(Violation::Hosts);
        }
        if self.hosts.iter().any(|h| h.len() > MAX_HOST) {
            return Err(Violation::HostLength);
        }
        self.observed.check()?;
        self.samples.check()
    }

    /// Schemas and volatility of the query, headers and bodies.
    pub fn observed(&self) -> &Observed {
        &self.observed
    }

    fn merge(&mut self, other: Stats) {
        for (status, n) in other.statuses {
            let mine = self.statuses.entry(status).or_default();
            *mine = mine.saturating_add(n);
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

    fn for_each_stats_mut(&mut self, f: &mut impl FnMut(&mut Stats)) {
        self.endpoints.values_mut().for_each(&mut *f);
        for child in self.literals.values_mut().chain(self.param.as_deref_mut()) {
            child.for_each_stats_mut(f);
        }
    }

    /// The node at exactly these segments.
    fn at(&self, segments: &[Seg]) -> Option<&Node> {
        let Some((first, rest)) = segments.split_first() else {
            return Some(self);
        };
        match first {
            Seg::Literal(literal) => self.literals.get(literal),
            Seg::Param => self.param.as_deref(),
        }?
        .at(rest)
    }

    fn at_mut(&mut self, segments: &[Seg]) -> Option<&mut Node> {
        let Some((first, rest)) = segments.split_first() else {
            return Some(self);
        };
        match first {
            Seg::Literal(literal) => self.literals.get_mut(literal),
            Seg::Param => self.param.as_deref_mut(),
        }?
        .at_mut(rest)
    }

    /// Where the endpoint at `segments` is once literals have been absorbed: a literal that
    /// is gone leads to the parameter child.
    fn route(&self, segments: &[Seg]) -> Option<Vec<Seg>> {
        let mut node = self;
        let mut out = Vec::with_capacity(segments.len());
        for segment in segments {
            match segment {
                Seg::Literal(literal) if node.literals.contains_key(literal) => {
                    out.push(segment.clone());
                    node = &node.literals[literal];
                }
                _ => {
                    out.push(Seg::Param);
                    node = node.param.as_deref()?;
                }
            }
        }
        Some(out)
    }

    /// The endpoints below are within the bounds, `segments` levels and `bytes` of path
    /// deep: a literal costs its bytes and a slash, a parameter a slash.
    fn check(&self, segments: usize, bytes: usize) -> Result<(), Violation> {
        for stats in self.endpoints.values() {
            stats.check()?;
        }
        let literals = self.literals.iter().map(|(l, c)| (l.len() + 1, c));
        let param = self.param.as_deref().map(|c| (1, c));
        for (cost, child) in literals.chain(param) {
            if segments + 1 > MAX_SEGMENTS {
                return Err(Violation::PathSegments);
            }
            if bytes + cost > MAX_PATH {
                return Err(Violation::PathLength);
            }
            child.check(segments + 1, bytes + cost)?;
        }
        Ok(())
    }

    /// Nothing below this node, and it does not absorb: it can go.
    fn prunable(&self) -> bool {
        self.endpoints.is_empty()
            && self.literals.is_empty()
            && self.param.is_none()
            && !self.absorbing
    }

    /// Where the endpoints are: the segments of their template, and the method.
    fn places(&self, prefix: &mut Vec<Seg>, out: &mut Vec<(Vec<Seg>, String)>) {
        for method in self.endpoints.keys() {
            out.push((prefix.clone(), method.clone()));
        }
        for (literal, child) in &self.literals {
            prefix.push(Seg::Literal(literal.clone()));
            child.places(prefix, out);
            prefix.pop();
        }
        if let Some(child) = &self.param {
            prefix.push(Seg::Param);
            child.places(prefix, out);
            prefix.pop();
        }
    }

    /// Removes the endpoint at `segments`, and the nodes it leaves empty.
    fn remove(&mut self, segments: &[Seg], method: &str) {
        let Some((first, rest)) = segments.split_first() else {
            self.endpoints.remove(method);
            return;
        };
        match first {
            Seg::Literal(literal) => {
                if let Some(child) = self.literals.get_mut(literal) {
                    child.remove(rest, method);
                    if child.prunable() {
                        self.literals.remove(literal);
                    }
                }
            }
            Seg::Param => {
                if let Some(child) = self.param.as_deref_mut() {
                    child.remove(rest, method);
                    if child.prunable() {
                        self.param = None;
                    }
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

impl Scope {
    fn check(&self) -> Result<(), Violation> {
        self.root.check(0, 0)?;
        for (path, methods) in &self.raw {
            if path.len() > MAX_PATH {
                return Err(Violation::PathLength);
            }
            if split_path(path).len() > MAX_SEGMENTS {
                return Err(Violation::PathSegments);
            }
            methods.values().try_for_each(Stats::check)?;
        }
        Ok(())
    }

    fn for_each_stats<'a>(&'a self, f: &mut impl FnMut(&'a Stats)) {
        self.root.for_each_stats(f);
        for methods in self.raw.values() {
            methods.values().for_each(&mut *f);
        }
    }

    fn for_each_stats_mut(&mut self, f: &mut impl FnMut(&mut Stats)) {
        self.root.for_each_stats_mut(f);
        for methods in self.raw.values_mut() {
            methods.values_mut().for_each(&mut *f);
        }
    }

    fn is_empty(&self) -> bool {
        self.root.prunable() && self.raw.is_empty()
    }
}

/// Where an endpoint is held in a scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Place {
    Tree(Vec<Seg>),
    Raw(String),
}

/// An endpoint's place in a report, ordered like [`Report::endpoints`].
pub(super) type EndpointKey = (Direction, u16, String, String, Place);

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

    fn for_each_stats_mut(&mut self, mut f: impl FnMut(&mut Stats)) {
        for scope in self.scopes.values_mut().flat_map(BTreeMap::values_mut) {
            scope.for_each_stats_mut(&mut f);
        }
    }

    /// The key of every endpoint, sorted.
    pub(super) fn endpoint_keys(&self) -> Vec<EndpointKey> {
        let mut keys = Vec::new();
        for (direction, ports) in &self.scopes {
            for (port, scope) in ports {
                let mut places = Vec::new();
                scope.root.places(&mut Vec::new(), &mut places);
                for (segments, method) in places {
                    let template = if segments.is_empty() {
                        "/".to_string()
                    } else {
                        render(&segments)
                    };
                    keys.push((*direction, *port, template, method, Place::Tree(segments)));
                }
                for (path, methods) in &scope.raw {
                    for method in methods.keys() {
                        keys.push((
                            *direction,
                            *port,
                            path.clone(),
                            method.clone(),
                            Place::Raw(path.clone()),
                        ));
                    }
                }
            }
        }
        keys.sort();
        keys
    }

    pub(super) fn remove_endpoint(&mut self, (direction, port, _, method, place): &EndpointKey) {
        let Some(ports) = self.scopes.get_mut(direction) else {
            return;
        };
        if let Some(scope) = ports.get_mut(port) {
            match place {
                Place::Tree(segments) => scope.root.remove(segments, method),
                Place::Raw(path) => {
                    if let Some(methods) = scope.raw.get_mut(path) {
                        methods.remove(method);
                        if methods.is_empty() {
                            scope.raw.remove(path);
                        }
                    }
                }
            }
            if scope.is_empty() {
                ports.remove(port);
            }
        }
        if ports.is_empty() {
            self.scopes.remove(direction);
        }
    }

    /// Keeps the `limit` endpoints that sort first (spec §6, merge).
    fn limit_endpoints(&mut self, limit: usize) {
        if self.endpoint_count() <= limit {
            return;
        }
        for key in self.endpoint_keys().iter().skip(limit) {
            self.remove_endpoint(key);
        }
    }

    /// Drops the samples with the highest content hash (then the later endpoint) until they
    /// fit in `budget` (spec §6, merge).
    fn limit_samples(&mut self, budget: usize) {
        let total = self.sample_bytes();
        if total <= budget {
            return;
        }
        // (hash, endpoint ordinal in tree order, bytes)
        let mut all = Vec::new();
        let mut ordinal = 0usize;
        self.for_each_stats(|stats| {
            all.extend(
                stats
                    .samples
                    .iter()
                    .map(|(hash, sample)| (hash, ordinal, sample.bytes())),
            );
            ordinal += 1;
        });
        all.sort_unstable_by_key(|(hash, ordinal, _)| std::cmp::Reverse((*hash, *ordinal)));
        let mut excess = total - budget;
        let mut dropped = BTreeSet::new();
        for (hash, ordinal, bytes) in all {
            if excess == 0 {
                break;
            }
            excess = excess.saturating_sub(bytes);
            dropped.insert((ordinal, hash));
        }
        let mut ordinal = 0usize;
        self.for_each_stats_mut(|stats| {
            stats
                .samples
                .retain(|hash| !dropped.contains(&(ordinal, hash)));
            ordinal += 1;
        });
    }

    /// The stats of the endpoint an exchange belongs to, when it is already known.
    fn find(&self, exchange: &Exchange<'_>) -> Option<&Stats> {
        let scope = self.scopes.get(&exchange.direction)?.get(&exchange.port)?;
        if exchange.grpc {
            return scope
                .raw
                .get(&redact::path(exchange.path))?
                .get(exchange.method);
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
            // Not templated, but redacted: the client chooses its `content-type`.
            scope.raw.insert(redact::path(exchange.path), methods);
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

    /// The union of `other` into `self`, absorbing literals past `max_literals`; nothing is
    /// dropped. The counters saturate.
    pub(super) fn union(&mut self, other: Report, max_literals: usize) {
        if self.pod != other.pod {
            self.pod = None;
        }
        self.exchanges = self.exchanges.saturating_add(other.exchanges);
        for (direction, ports) in other.scopes {
            let mine = self.scopes.entry(direction).or_default();
            for (port, scope) in ports {
                mine.entry(port).or_default().merge(scope, max_literals);
            }
        }
    }

    /// Merges `other` into `self`, then enforces `max_endpoints` and `sample_budget`: the
    /// reports of other pods are not trusted to respect them.
    pub fn merge(&mut self, limits: &Limits, other: Report) {
        self.union(other, limits.max_literals);
        self.limit_endpoints(limits.max_endpoints);
        self.limit_samples(limits.sample_budget);
    }

    /// Sets `last_seen` to `day` on every endpoint and sample (spec §7.3, stamping).
    pub fn stamp(&mut self, _day: u64) {}

    /// The §6 redaction applied again to every sample, host and volatile field (spec §7.3,
    /// re-redaction), so that a report cannot hold what `redactor` redacts.
    pub fn re_redact(&mut self, _redactor: &Redactor) {}

    pub(super) fn clear_pod(&mut self) {
        self.pod = None;
    }

    pub(super) fn stats_at(
        &self,
        (direction, port, _, method, place): &EndpointKey,
    ) -> Option<&Stats> {
        let scope = self.scopes.get(direction)?.get(port)?;
        match place {
            Place::Tree(segments) => scope.root.at(segments)?.endpoints.get(method),
            Place::Raw(path) => scope.raw.get(path)?.get(method),
        }
    }

    pub(super) fn stats_at_mut(
        &mut self,
        (direction, port, _, method, place): &EndpointKey,
    ) -> Option<&mut Stats> {
        let scope = self.scopes.get_mut(direction)?.get_mut(port)?;
        match place {
            Place::Tree(segments) => scope.root.at_mut(segments)?.endpoints.get_mut(method),
            Place::Raw(path) => scope.raw.get_mut(path)?.get_mut(method),
        }
    }

    /// The key that an endpoint of an earlier report has in this one, which is its union
    /// with others: absorbed literals lead to their parameter. `None` when it is gone.
    pub(super) fn route(
        &self,
        (direction, port, template, method, place): &EndpointKey,
    ) -> Option<EndpointKey> {
        let scope = self.scopes.get(direction)?.get(port)?;
        let (template, place) = match place {
            Place::Tree(segments) => {
                let routed = scope.root.route(segments)?;
                let template = if routed.is_empty() {
                    "/".to_string()
                } else {
                    render(&routed)
                };
                (template, Place::Tree(routed))
            }
            Place::Raw(path) => {
                scope.raw.get(path)?;
                (template.clone(), Place::Raw(path.clone()))
            }
        };
        let key = (*direction, *port, template, method.clone(), place);
        self.stats_at(&key).map(|_| key)
    }

    /// Within the bounds of spec §6 and, per direction, at most `max_push_endpoints`
    /// endpoints: what the recorder of a sidecar can produce (spec §7.3, structural check).
    pub fn check(&self, max_push_endpoints: usize) -> Result<(), Violation> {
        if self.version != 1 {
            return Err(Violation::Version);
        }
        for ports in self.scopes.values() {
            let mut endpoints = 0usize;
            for scope in ports.values() {
                scope.for_each_stats(&mut |_| endpoints += 1);
            }
            if endpoints > max_push_endpoints {
                return Err(Violation::PushLimit);
            }
        }
        self.scopes
            .values()
            .flat_map(BTreeMap::values)
            .try_for_each(Scope::check)
    }

    /// Parses a pushed body, then checks it. Never panics, whatever the input.
    pub fn parse_checked(json: &str, max_push_endpoints: usize) -> Result<Self, Violation> {
        let report: Report = serde_json::from_str(json).map_err(|e| {
            if e.is_data() && e.to_string().starts_with("unknown field") {
                Violation::UnknownField
            } else {
                Violation::Malformed
            }
        })?;
        report.check(max_push_endpoints)?;
        Ok(report)
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
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
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
