//! The local report (spec §6): endpoints keyed by direction, port, method and path template,
//! with status counters, a latency histogram and the hosts seen. Merging is pure,
//! commutative and associative; recording an exchange merges a one-exchange report.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use super::check::Violation;
use super::histogram::Histogram;
use super::observed::{Observation, Observed, state_bound};
use super::redact::{self, MARKER, Redactor, value_rule};
use super::sample::{MAX_SAMPLES, Sample, Samples};
use super::scope::{self, ScopeKey};
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
    /// `MIMIC_DISCOVERY_MAX_ENDPOINTS`, for each direction.
    pub max_endpoints: usize,
    /// `MIMIC_DISCOVERY_MAX_AUTHORITIES`: outbound scopes held.
    pub max_authorities: usize,
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
    /// A new endpoint past `max_endpoints` in its direction.
    EndpointLimit,
    /// An outbound exchange to a new authority past `max_authorities`.
    AuthorityLimit,
}

/// What `record_tunnel` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tunneled {
    Counted,
    /// A new authority past `max_authorities`.
    AuthorityLimit,
    /// A kind outside [`TUNNEL_KINDS`], or a key that is not an authority.
    Invalid,
}

/// How an outbound exchange reached its destination (spec §6.1, `origin`).
#[derive(Debug, Clone, Copy)]
pub struct Via<'a> {
    /// Intercepted TLS (`https`), else cleartext (`http`).
    pub tls: bool,
    /// The SNI of the intercepted ClientHello.
    pub sni: Option<&'a str>,
    /// The original destination.
    pub dst: Option<SocketAddr>,
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
    /// Outbound scopes only (spec §6.1): where the exchanges went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<Origin>,
    /// Outbound scopes only: connections that were counted, not recorded, by kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tunnels: Option<BTreeMap<String, u64>>,
    /// Outbound scopes only, set by the controller (spec §7.3, stamping): the last day the
    /// scope, its `origin` and its `tunnels` were pushed; a merge keeps the larger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<u64>,
}

/// Most distinct SNIs and destinations kept per outbound scope.
const MAX_ORIGINS: usize = 16;

/// The kinds of `tunnels` (spec §6.1).
pub const TUNNEL_KINDS: [&str; 8] = [
    "tcp",
    "connect",
    "tls_excluded",
    "tls_no_ca",
    "tls_unsupported",
    "tls_upstream_untrusted",
    "tls_client_cert_requested",
    "tls_client_rejected",
];

/// What an outbound scope knows of where its exchanges went (spec §6.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    /// Exchanges per scheme, `http` and `https`.
    schemes: BTreeMap<String, u64>,
    sni: BTreeSet<String>,
    dst: BTreeSet<String>,
}

impl Origin {
    /// The origin of one exchange: one count under its scheme, the SNI and destination when
    /// they are plain (a name in the grammar that no detector catches, an address).
    fn of(via: &Via<'_>) -> Self {
        let scheme = if via.tls { "https" } else { "http" };
        let mut origin = Origin {
            schemes: BTreeMap::from([(scheme.to_string(), 1)]),
            ..Origin::default()
        };
        if let Some(sni) = via.sni
            && !scope::caught(sni)
            && let Some(sni) = scope::dns_host(sni)
        {
            origin.sni.insert(sni);
        }
        if let Some(dst) = via.dst {
            // As in the scope key (spec §6.1): the grammar has no room for port 0.
            let port = dst.port().max(1);
            origin.dst.insert(match dst.ip().to_canonical() {
                IpAddr::V4(v4) => format!("{v4}:{port}"),
                IpAddr::V6(v6) => format!("[{v6}]:{port}"),
            });
        }
        origin
    }

    /// Within the bounds of §6.1: what a sidecar can have recorded.
    fn check(&self) -> Result<(), Violation> {
        let plain = self
            .schemes
            .keys()
            .all(|s| matches!(s.as_str(), "http" | "https"))
            && self.sni.len() <= MAX_ORIGINS
            && self.dst.len() <= MAX_ORIGINS
            && self.sni.iter().all(|v| scope::valid_sni(v))
            && self.dst.iter().all(|v| scope::valid_dst(v));
        if plain {
            Ok(())
        } else {
            Err(Violation::Origin)
        }
    }

    fn merge(&mut self, other: Origin) {
        for (scheme, n) in other.schemes {
            let count = self.schemes.entry(scheme).or_default();
            *count = count.saturating_add(n);
        }
        self.sni.extend(other.sni);
        self.dst.extend(other.dst);
        while self.sni.len() > MAX_ORIGINS {
            self.sni.pop_last();
        }
        while self.dst.len() > MAX_ORIGINS {
            self.dst.pop_last();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "WireReport")]
pub struct Report {
    version: u32,
    pod: Option<String>,
    exchanges: u64,
    /// The exchanges of the outbound scopes, a part of `exchanges` (spec §6.1). Kept when
    /// the scopes are gone (evicted, expired) so that the split stays exact; written in
    /// version 2 only.
    outbound_exchanges: u64,
    scopes: BTreeMap<Direction, BTreeMap<ScopeKey, Scope>>,
}

/// What `Report` serialises as: `outbound_exchanges` is written in version 2 only.
#[derive(Serialize)]
struct WireRef<'a> {
    version: u32,
    pod: &'a Option<String>,
    exchanges: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    outbound_exchanges: Option<u64>,
    scopes: &'a BTreeMap<Direction, BTreeMap<ScopeKey, Scope>>,
}

/// What `Report` is read from; an absent `outbound_exchanges` is 0.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReport {
    version: u32,
    pod: Option<String>,
    exchanges: u64,
    #[serde(default)]
    outbound_exchanges: u64,
    scopes: BTreeMap<Direction, BTreeMap<ScopeKey, Scope>>,
}

impl From<WireReport> for Report {
    fn from(w: WireReport) -> Self {
        Report {
            version: w.version,
            pod: w.pod,
            exchanges: w.exchanges,
            outbound_exchanges: w.outbound_exchanges,
            scopes: w.scopes,
        }
    }
}

impl Serialize for Report {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireRef {
            version: self.version,
            pod: &self.pod,
            exchanges: self.exchanges,
            outbound_exchanges: (self.version == 2).then_some(self.outbound_exchanges),
            scopes: &self.scopes,
        }
        .serialize(serializer)
    }
}

/// An endpoint of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint<'a> {
    pub direction: Direction,
    /// The port of the scope: the listening port, or the one the authority ends with.
    pub port: u16,
    /// The scope key of an outbound endpoint (spec §6.1); `None` for inbound.
    pub authority: Option<&'a str>,
    pub method: &'a str,
    pub template: String,
    /// The segments of the template; empty for a gRPC path, which is never templated.
    pub segs: Vec<Seg>,
    /// A gRPC path (`content-type: application/grpc*`).
    pub grpc: bool,
    pub stats: &'a Stats,
}

/// An endpoint on its way to `Endpoint`.
struct Found<'a> {
    template: String,
    segs: Vec<Seg>,
    grpc: bool,
    method: &'a str,
    stats: &'a Stats,
}

impl Stats {
    /// Counters, latency and host of one exchange.
    fn of(exchange: &Exchange<'_>) -> Self {
        let mut s = Stats::default();
        *s.statuses.entry(exchange.status).or_default() += 1;
        s.latency.observe(exchange.head_ms);
        // An authority has no userinfo (spec §4.1), but it is redacted like any other value.
        let host = exchange
            .host
            .filter(|_| exchange.direction == Direction::Inbound);
        s.hosts.extend(host.map(|h| {
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
        self.last_seen = self.last_seen.max(other.last_seen);
    }

    /// The §6 redaction again on the hosts, the samples and the volatile fields.
    fn re_redact(&mut self, request_redactor: &Redactor, response_redactor: &Redactor) {
        self.hosts = std::mem::take(&mut self.hosts)
            .into_iter()
            .map(|h| {
                if value_rule(&h).is_some() {
                    MARKER.to_string()
                } else {
                    h
                }
            })
            .collect();
        let changed = self.samples.re_redact(request_redactor, response_redactor);
        self.observed
            .re_redact(request_redactor, response_redactor, &changed);
    }
}

fn merge_methods(into: &mut BTreeMap<String, Stats>, from: BTreeMap<String, Stats>) {
    for (method, stats) in from {
        into.entry(method).or_default().merge(stats);
    }
}

impl Node {
    fn re_redact_paths(&mut self) {
        if let Some(param) = &mut self.param {
            param.re_redact_paths();
        }
        for (literal, mut child) in std::mem::take(&mut self.literals) {
            child.re_redact_paths();
            self.literals
                .entry(redact::segment(&literal))
                .or_default()
                .merge(child, usize::MAX);
        }
    }

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

    fn collect<'a>(&'a self, prefix: &mut Vec<Seg>, out: &mut Vec<Found<'a>>) {
        let template = render(prefix);
        for (method, stats) in &self.endpoints {
            out.push(Found {
                template: if prefix.is_empty() {
                    "/".into()
                } else {
                    template.clone()
                },
                segs: prefix.clone(),
                grpc: false,
                method,
                stats,
            });
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

/// What an outbound scope holds besides its endpoints (spec §6.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundMeta<'a> {
    /// `http` and `https`, those with a count, in that order.
    pub schemes: Vec<&'a str>,
    pub sni: Vec<&'a str>,
    pub dst: Vec<&'a str>,
    /// Connections counted, not recorded, by kind.
    pub tunnels: BTreeMap<&'a str, u64>,
}

impl Scope {
    fn meta(&self) -> OutboundMeta<'_> {
        let mut meta = OutboundMeta::default();
        if let Some(origin) = &self.origin {
            meta.schemes = ["http", "https"]
                .into_iter()
                .filter(|s| origin.schemes.get(*s).is_some_and(|n| *n > 0))
                .collect();
            meta.sni = origin.sni.iter().map(String::as_str).collect();
            meta.dst = origin.dst.iter().map(String::as_str).collect();
        }
        if let Some(tunnels) = &self.tunnels {
            meta.tunnels = tunnels.iter().map(|(k, n)| (k.as_str(), *n)).collect();
        }
        meta
    }
}

impl Scope {
    fn merge(&mut self, other: Scope, max_literals: usize) {
        self.last_seen = self.last_seen.max(other.last_seen);
        self.root.merge(other.root, max_literals);
        for (path, methods) in other.raw {
            merge_methods(self.raw.entry(path).or_default(), methods);
        }
        if let Some(origin) = other.origin {
            self.origin
                .get_or_insert_with(Origin::default)
                .merge(origin);
        }
        if let Some(tunnels) = other.tunnels {
            let mine = self.tunnels.get_or_insert_with(BTreeMap::new);
            for (kind, n) in tunnels {
                let count = mine.entry(kind).or_default();
                *count = count.saturating_add(n);
            }
        }
    }
}

impl Scope {
    /// `origin` and `tunnels` only on outbound scopes, within their bounds; no `hosts` on an
    /// outbound endpoint.
    fn egress_check(&self, outbound: bool) -> Result<(), Violation> {
        if !outbound {
            return if self.origin.is_none() && self.tunnels.is_none() && self.last_seen.is_none() {
                Ok(())
            } else {
                Err(Violation::Malformed)
            };
        }
        self.origin.as_ref().map_or(Ok(()), Origin::check)?;
        if let Some(tunnels) = &self.tunnels
            && !tunnels.keys().all(|k| TUNNEL_KINDS.contains(&k.as_str()))
        {
            return Err(Violation::Tunnels);
        }
        let mut hosts = false;
        self.for_each_stats(&mut |s| hosts |= !s.hosts.is_empty());
        if hosts { Err(Violation::Hosts) } else { Ok(()) }
    }

    /// The value detectors again on the literal segments and the gRPC paths; keys that become
    /// equal merge.
    fn re_redact_paths(&mut self) {
        self.root.re_redact_paths();
        for (path, methods) in std::mem::take(&mut self.raw) {
            merge_methods(self.raw.entry(redact::path(&path)).or_default(), methods);
        }
    }

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

    /// What the scope carried: the exchanges of its endpoints plus its tunnel counts.
    fn traffic(&self) -> u64 {
        let mut n = 0u64;
        self.for_each_stats(&mut |s| n = n.saturating_add(s.total()));
        let tunnels = self.tunnels.iter().flat_map(BTreeMap::values);
        tunnels.fold(n, |n, t| n.saturating_add(*t))
    }

    fn is_empty(&self) -> bool {
        self.root.prunable()
            && self.raw.is_empty()
            && self.tunnels.as_ref().is_none_or(BTreeMap::is_empty)
    }
}

/// Where an endpoint is held in a scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Place {
    Tree(Vec<Seg>),
    Raw(String),
}

/// An endpoint's place in a report, ordered like [`Report::endpoints`].
pub(super) type EndpointKey = (Direction, ScopeKey, String, String, Place);

impl Report {
    pub fn new(pod: Option<String>) -> Self {
        Report {
            version: 1,
            pod,
            exchanges: 0,
            outbound_exchanges: 0,
            scopes: BTreeMap::new(),
        }
    }

    pub fn exchanges(&self) -> u64 {
        self.exchanges
    }

    /// The part of [`Report::exchanges`] that went to outbound scopes (spec §6.1).
    pub fn outbound_exchanges(&self) -> u64 {
        self.outbound_exchanges
    }

    /// A version 1 report carries no outbound exchange, and no report more than it counts.
    fn counter_ok(&self) -> bool {
        self.outbound_exchanges <= self.exchanges
            && (self.version == 2 || self.outbound_exchanges == 0)
    }

    /// The endpoints of one direction, or of both.
    fn for_each_stats_of<'a>(&'a self, only: Option<Direction>, mut f: impl FnMut(&'a Stats)) {
        for (_, scopes) in self
            .scopes
            .iter()
            .filter(|(d, _)| only.is_none_or(|o| o == **d))
        {
            scopes
                .values()
                .for_each(|scope| scope.for_each_stats(&mut f));
        }
    }

    fn for_each_stats_mut(&mut self, f: impl FnMut(&mut Stats)) {
        self.for_each_stats_of_mut(None, f);
    }

    fn for_each_stats_of_mut(&mut self, only: Option<Direction>, mut f: impl FnMut(&mut Stats)) {
        for (_, scopes) in self
            .scopes
            .iter_mut()
            .filter(|(d, _)| only.is_none_or(|o| o == **d))
        {
            scopes
                .values_mut()
                .for_each(|scope| scope.for_each_stats_mut(&mut f));
        }
    }

    /// The key of every endpoint, sorted.
    pub(super) fn endpoint_keys(&self) -> Vec<EndpointKey> {
        let mut keys = Vec::new();
        for (direction, ports) in &self.scopes {
            for (scope_key, scope) in ports {
                let mut places = Vec::new();
                scope.root.places(&mut Vec::new(), &mut places);
                for (segments, method) in places {
                    let template = if segments.is_empty() {
                        "/".to_string()
                    } else {
                        render(&segments)
                    };
                    keys.push((
                        *direction,
                        scope_key.clone(),
                        template,
                        method,
                        Place::Tree(segments),
                    ));
                }
                for (path, methods) in &scope.raw {
                    for method in methods.keys() {
                        keys.push((
                            *direction,
                            scope_key.clone(),
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

    pub(super) fn remove_endpoint(&mut self, (direction, key, _, method, place): &EndpointKey) {
        let Some(ports) = self.scopes.get_mut(direction) else {
            return;
        };
        if let Some(scope) = ports.get_mut(key) {
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
            // An outbound scope stays with its origin and tunnels: evicting endpoints must not
            // change what the merge keeps of them (spec §6.1), and `max_authorities` bounds them.
            if *direction == Direction::Inbound && scope.is_empty() {
                ports.remove(key);
            }
        }
        if ports.is_empty() {
            self.scopes.remove(direction);
        }
        self.sync_version();
    }

    /// Keeps the `limit` endpoints that sort first (spec §6, merge).
    fn limit_endpoints(&mut self, limit: usize) {
        for direction in [Direction::Inbound, Direction::Outbound] {
            if self.endpoint_count(direction) <= limit {
                continue;
            }
            let keys: Vec<EndpointKey> = self
                .endpoint_keys()
                .into_iter()
                .filter(|k| k.0 == direction)
                .collect();
            for key in keys.iter().skip(limit) {
                self.remove_endpoint(key);
            }
        }
    }

    /// Keeps the `limit` outbound scopes that sort first (spec §6.1, merge).
    fn limit_authorities(&mut self, limit: usize) {
        if let Some(scopes) = self.scopes.get_mut(&Direction::Outbound) {
            while scopes.len() > limit {
                scopes.pop_last();
            }
            if scopes.is_empty() {
                self.scopes.remove(&Direction::Outbound);
            }
        }
    }

    /// Drops the samples with the highest content hash (then the later endpoint) until they
    /// fit in `budget` (spec §6, merge).
    fn limit_samples(&mut self, budget: usize) {
        // Outbound samples hold at most half of the budget (spec §6.1).
        self.limit_samples_of(Some(Direction::Outbound), budget / 2);
        self.limit_samples_of(None, budget);
    }

    fn limit_samples_of(&mut self, only: Option<Direction>, budget: usize) {
        let total = self.sample_bytes_of(only);
        if total <= budget {
            return;
        }
        // (hash, endpoint ordinal in tree order, bytes)
        let mut all = Vec::new();
        let mut ordinal = 0usize;
        self.for_each_stats_of(only, |stats| {
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
        self.for_each_stats_of_mut(only, |stats| {
            stats
                .samples
                .retain(|hash| !dropped.contains(&(ordinal, hash)));
            ordinal += 1;
        });
    }

    /// The stats of the endpoint an exchange belongs to, when it is already known.
    fn find(&self, key: &ScopeKey, exchange: &Exchange<'_>) -> Option<&Stats> {
        let scope = self.scopes.get(&exchange.direction)?.get(key)?;
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

    /// Endpoints held in a direction, whatever their scope.
    fn endpoint_count(&self, direction: Direction) -> usize {
        let mut n = 0;
        self.for_each_stats_of(Some(direction), |_| n += 1);
        n
    }

    /// Outbound scopes held (`mimic_discovery_authorities`).
    pub fn authorities(&self) -> usize {
        self.outbound_scopes()
    }

    /// The outbound scopes by authority, with what they know besides their endpoints (spec
    /// §6.1): schemes seen, SNIs, destinations and tunnel counts.
    pub fn outbound_meta(&self) -> BTreeMap<&str, OutboundMeta<'_>> {
        let scopes = self.scopes.get(&Direction::Outbound).into_iter().flatten();
        scopes
            .filter_map(|(key, scope)| match key {
                ScopeKey::Authority(authority) => Some((authority.as_str(), scope.meta())),
                ScopeKey::Port(_) => None,
            })
            .collect()
    }

    fn outbound_scopes(&self) -> usize {
        self.scopes
            .get(&Direction::Outbound)
            .map_or(0, BTreeMap::len)
    }

    /// Bytes of the samples held (what `MIMIC_DISCOVERY_SAMPLE_BUDGET` limits).
    pub fn sample_bytes(&self) -> usize {
        self.sample_bytes_of(None)
    }

    fn sample_bytes_of(&self, only: Option<Direction>) -> usize {
        let mut n = 0;
        self.for_each_stats_of(only, |s| n = usize::saturating_add(n, s.samples.bytes()));
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
        // Both directions hold `max_endpoints`; each outbound scope adds its origin
        // (16 SNIs and 16 destinations), its tunnels and its framing.
        let per_authority = 300 + 2 * MAX_ORIGINS * 300 + TUNNEL_KINDS.len() * 60 + 256;
        limits
            .max_endpoints
            .saturating_mul(2)
            .saturating_mul(per_endpoint)
            .saturating_add(limits.max_authorities.saturating_mul(per_authority))
            + 1024
    }

    /// Counts a connection that carried no recordable HTTP in the `tunnels` of the outbound
    /// scope `key` (spec §6.1): no endpoint, no sample.
    pub fn record_tunnel(&mut self, limits: &Limits, key: ScopeKey, kind: &str) -> Tunneled {
        let Some(kind) = TUNNEL_KINDS.iter().find(|k| **k == kind) else {
            return Tunneled::Invalid;
        };
        if !matches!(key, ScopeKey::Authority(_)) {
            return Tunneled::Invalid;
        }
        if self.authority_refused(&key, limits) {
            return Tunneled::AuthorityLimit;
        }
        let scope = Scope {
            origin: Some(Origin::default()),
            tunnels: Some(BTreeMap::from([((*kind).to_string(), 1)])),
            ..Scope::default()
        };
        let mut one = Report::new(self.pod.clone());
        one.scopes
            .entry(Direction::Outbound)
            .or_default()
            .insert(key, scope);
        self.merge(limits, one);
        Tunneled::Counted
    }

    /// A new outbound scope past `max_authorities`.
    fn authority_refused(&self, key: &ScopeKey, limits: &Limits) -> bool {
        self.outbound_scopes() >= limits.max_authorities
            && !self
                .scopes
                .get(&Direction::Outbound)
                .is_some_and(|scopes| scopes.contains_key(key))
    }

    /// Records one exchange: the same as merging a one-exchange report, once the bounds
    /// (spec §6) allow it.
    pub fn record(
        &mut self,
        limits: &Limits,
        redactor: &Redactor,
        exchange: &Exchange<'_>,
    ) -> Outcome {
        let via = Via {
            tls: false,
            sni: None,
            dst: None,
        };
        self.record_at(
            ScopeKey::Port(exchange.port),
            &via,
            limits,
            redactor,
            exchange,
        )
    }

    /// [`Report::record`] in the scope `key`: the port, or the authority of an outbound
    /// exchange (spec §6.1).
    pub fn record_at(
        &mut self,
        key: ScopeKey,
        via: &Via<'_>,
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
        if matches!(key, ScopeKey::Authority(_)) && self.authority_refused(&key, limits) {
            return Outcome::AuthorityLimit;
        }
        let known = self.find(&key, exchange);
        // Inbound scanners make `404` and `405` cheap; an outbound call was chosen by the app
        // and a mock must answer it as the destination did (spec §6.1).
        let counted_only =
            exchange.direction == Direction::Inbound && matches!(exchange.status, 404 | 405);
        if known.is_none() {
            if counted_only {
                return Outcome::NotFound;
            }
            if self.endpoint_count(exchange.direction) >= limits.max_endpoints {
                return Outcome::EndpointLimit;
            }
        }
        let new_endpoint = known.is_none();
        let mut stats = Stats::of(exchange);
        let mut sampled = Sampled::None;
        if !counted_only {
            // An outbound request carries the app's own credentials (spec §6.1).
            let outbound = redactor.outbound_request();
            let request_redactor = if exchange.direction == Direction::Outbound {
                &outbound
            } else {
                redactor
            };
            let (request, response) =
                stats
                    .observed
                    .record_parsed(request_redactor, redactor, &exchange.observation);
            let (hash, sample) =
                Sample::new(request_redactor, redactor, exchange, request, response);
            let present = known.is_some_and(|k| k.samples.contains(hash));
            let bytes = sample.bytes();
            let fits = self.sample_bytes().saturating_add(bytes) <= limits.sample_budget
                && (exchange.direction == Direction::Inbound
                    || self
                        .sample_bytes_of(Some(Direction::Outbound))
                        .saturating_add(bytes)
                        <= limits.sample_budget / 2);
            if present || fits {
                stats.samples = Samples::single(hash, sample);
                sampled = Sampled::Admitted;
            } else {
                sampled = Sampled::Budget;
            }
        }
        let methods = BTreeMap::from([(exchange.method.to_string(), stats)]);
        let mut scope = Scope::default();
        if matches!(key, ScopeKey::Authority(_)) {
            scope.origin = Some(Origin::of(via));
            scope.tunnels = Some(BTreeMap::new());
        }
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
        one.outbound_exchanges = u64::from(exchange.direction == Direction::Outbound);
        one.scopes
            .entry(exchange.direction)
            .or_default()
            .insert(key, scope);
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
        self.outbound_exchanges = self
            .outbound_exchanges
            .saturating_add(other.outbound_exchanges);
        for (direction, ports) in other.scopes {
            let mine = self.scopes.entry(direction).or_default();
            for (key, scope) in ports {
                mine.entry(key).or_default().merge(scope, max_literals);
            }
        }
        self.sync_version();
    }

    /// Whether the report holds an inbound scope (an outbound part holds none).
    pub fn holds_inbound(&self) -> bool {
        self.scopes
            .get(&Direction::Inbound)
            .is_some_and(|scopes| !scopes.is_empty())
    }

    fn has_authorities(&self) -> bool {
        self.scopes
            .get(&Direction::Outbound)
            .is_some_and(|scopes| scopes.keys().any(|k| matches!(k, ScopeKey::Authority(_))))
    }

    /// Version 2 when the report holds an outbound scope, else 1 (spec §6.1). A report that
    /// holds none no longer counts the exchanges of its outbound scopes: they go with them.
    fn sync_version(&mut self) {
        if self.has_authorities() {
            self.version = 2;
        } else {
            self.version = 1;
            self.exchanges = self.exchanges.saturating_sub(self.outbound_exchanges);
            self.outbound_exchanges = 0;
        }
    }

    /// Merges `other` into `self`, then enforces `max_endpoints` and `sample_budget`: the
    /// reports of other pods are not trusted to respect them.
    pub fn merge(&mut self, limits: &Limits, other: Report) {
        self.union(other, limits.max_literals);
        self.limit_authorities(limits.max_authorities);
        self.limit_endpoints(limits.max_endpoints);
        self.limit_samples(limits.sample_budget);
        self.sync_version();
    }

    /// Removes every sample, keeping counters, schemas and the shape (the `413` resend of
    /// spec §7.2).
    pub fn drop_samples(&mut self) {
        self.for_each_stats_mut(|stats| stats.samples = Samples::default());
    }

    /// Sets `last_seen` to `day` on every endpoint and sample (spec §7.3, stamping).
    pub fn stamp(&mut self, day: u64) {
        self.for_each_stats_mut(|stats| {
            stats.last_seen = Some(day);
            stats.samples.stamp(day);
        });
        for scope in self
            .scopes
            .entry(Direction::Outbound)
            .or_default()
            .values_mut()
        {
            scope.last_seen = Some(day);
        }
        if self
            .scopes
            .get(&Direction::Outbound)
            .is_some_and(BTreeMap::is_empty)
        {
            self.scopes.remove(&Direction::Outbound);
        }
    }

    /// The traffic of every outbound scope: the exchanges of its endpoints plus its `tunnels`
    /// counts (spec §7.3, bounded merge).
    pub(super) fn outbound_traffic(&self) -> Vec<(ScopeKey, u64)> {
        self.scopes
            .get(&Direction::Outbound)
            .into_iter()
            .flatten()
            .map(|(key, scope)| (key.clone(), scope.traffic()))
            .collect()
    }

    pub(super) fn outbound_keys(&self) -> Vec<ScopeKey> {
        self.scopes
            .get(&Direction::Outbound)
            .into_iter()
            .flat_map(|scopes| scopes.keys().cloned())
            .collect()
    }

    /// Removes an outbound scope with its endpoints, samples, `origin` and `tunnels`.
    pub(super) fn remove_authority(&mut self, key: &ScopeKey) {
        if let Some(scopes) = self.scopes.get_mut(&Direction::Outbound) {
            scopes.remove(key);
            if scopes.is_empty() {
                self.scopes.remove(&Direction::Outbound);
            }
        }
        self.sync_version();
    }

    /// Removes the outbound scopes that hold no endpoint and whose `last_seen` is `stale`
    /// (spec §7.3, retention); how many.
    pub(super) fn drop_stale_authorities(&mut self, stale: impl Fn(Option<u64>) -> bool) -> u64 {
        let doomed: Vec<ScopeKey> = self
            .scopes
            .get(&Direction::Outbound)
            .into_iter()
            .flatten()
            .filter(|(_, scope)| {
                let mut endpoints = false;
                scope.for_each_stats(&mut |_| endpoints = true);
                !endpoints && stale(scope.last_seen)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &doomed {
            self.remove_authority(key);
        }
        doomed.len() as u64
    }

    /// The §6 redaction applied again to every sample, host and volatile field (spec §7.3,
    /// re-redaction), so that a report cannot hold what `redactor` redacts.
    pub fn re_redact(&mut self, redactor: &Redactor) {
        for scope in self.scopes.values_mut().flat_map(BTreeMap::values_mut) {
            scope.re_redact_paths();
        }
        self.re_redact_authorities();
        // The outbound request view of §6.1 applies again to the outbound requests.
        let outbound = redactor.outbound_request();
        self.for_each_stats_of_mut(Some(Direction::Inbound), |stats| {
            stats.re_redact(redactor, redactor);
        });
        self.for_each_stats_of_mut(Some(Direction::Outbound), |stats| {
            stats.re_redact(&outbound, redactor);
        });
    }

    /// The authority hosts (scope keys) and the SNIs of the outbound scopes, redacted again:
    /// scopes whose key becomes equal merge, a caught SNI is removed.
    fn re_redact_authorities(&mut self) {
        let Some(scopes) = self.scopes.get_mut(&Direction::Outbound) else {
            return;
        };
        for (key, mut scope) in std::mem::take(scopes) {
            if let Some(origin) = &mut scope.origin {
                origin.sni.retain(|sni| !scope::caught(sni));
            }
            let key = match key {
                ScopeKey::Authority(authority) => {
                    ScopeKey::Authority(scope::redact_authority(&authority))
                }
                port => port,
            };
            // The bound on literals applies again when the report is merged into the store.
            scopes.entry(key).or_default().merge(scope, usize::MAX);
        }
    }

    /// The inbound part (`"version": 1`, what an F7 controller stores) and the outbound part
    /// (`"version": 2`, outbound scopes only; none without outbound scopes) of a report
    /// (spec §7.3, write).
    pub fn split(&self) -> (Report, Option<Report>) {
        self.clone().into_split()
    }

    /// [`Report::split`] without copying the scopes: the controller's write holds a report of
    /// tens of megabytes at the store limits (SHR-18).
    pub fn into_split(mut self) -> (Report, Option<Report>) {
        let part =
            |direction: Direction, version: u32, scopes: Option<BTreeMap<ScopeKey, Scope>>| {
                Report {
                    version,
                    pod: self.pod.clone(),
                    exchanges: 0,
                    outbound_exchanges: 0,
                    scopes: scopes
                        .map(|scopes| BTreeMap::from([(direction, scopes)]))
                        .unwrap_or_default(),
                }
            };
        let inbound_scopes = self.scopes.remove(&Direction::Inbound);
        let outbound_scopes = self.scopes.remove(&Direction::Outbound);
        let mut inbound = part(Direction::Inbound, 1, inbound_scopes);
        let mut outbound = part(Direction::Outbound, 2, outbound_scopes);
        // A report counts its exchanges once: the outbound part takes the counter of the
        // outbound ones (also those of scopes that are gone), the inbound part the rest, so
        // that the parts add up to the report.
        outbound.exchanges = self.outbound_exchanges.min(self.exchanges);
        outbound.outbound_exchanges = outbound.exchanges;
        inbound.exchanges = self.exchanges - outbound.exchanges;
        (inbound, (!outbound.scopes.is_empty()).then_some(outbound))
    }

    /// The report that `split` took apart.
    pub fn join(mut inbound: Report, outbound: Option<Report>) -> Report {
        if let Some(outbound) = outbound {
            inbound.union(outbound, usize::MAX);
        }
        inbound
    }

    pub(super) fn clear_pod(&mut self) {
        self.pod = None;
    }

    pub(super) fn stats_at(
        &self,
        (direction, key, _, method, place): &EndpointKey,
    ) -> Option<&Stats> {
        let scope = self.scopes.get(direction)?.get(key)?;
        match place {
            Place::Tree(segments) => scope.root.at(segments)?.endpoints.get(method),
            Place::Raw(path) => scope.raw.get(path)?.get(method),
        }
    }

    pub(super) fn stats_at_mut(
        &mut self,
        (direction, key, _, method, place): &EndpointKey,
    ) -> Option<&mut Stats> {
        let scope = self.scopes.get_mut(direction)?.get_mut(key)?;
        match place {
            Place::Tree(segments) => scope.root.at_mut(segments)?.endpoints.get_mut(method),
            Place::Raw(path) => scope.raw.get_mut(path)?.get_mut(method),
        }
    }

    /// The key that an endpoint of an earlier report has in this one, which is its union
    /// with others: absorbed literals lead to their parameter. `None` when it is gone.
    pub(super) fn route(
        &self,
        (direction, key, template, method, place): &EndpointKey,
    ) -> Option<EndpointKey> {
        let scope = self.scopes.get(direction)?.get(key)?;
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
        let routed = (*direction, key.clone(), template, method.clone(), place);
        self.stats_at(&routed).map(|_| routed)
    }

    /// 1, or 2 when the report holds outbound scopes (spec §6.1).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Within the bounds of spec §6 and, per direction, at most `max_push_endpoints`
    /// endpoints: what the recorder of a sidecar can produce (spec §7.3, structural check).
    pub fn check(
        &self,
        max_push_endpoints: usize,
        max_authorities: usize,
    ) -> Result<(), Violation> {
        let outbound = self.scopes.get(&Direction::Outbound);
        if outbound
            .is_some_and(|scopes| scopes.keys().any(|k| !matches!(k, ScopeKey::Authority(_))))
        {
            return Err(Violation::Authority);
        }
        // Version 2 iff the report holds an outbound scope (spec §6.1).
        if self.version != if self.has_authorities() { 2 } else { 1 } {
            return Err(Violation::Version);
        }
        if !self.counter_ok() {
            return Err(Violation::Malformed);
        }
        if self.scopes.get(&Direction::Inbound).is_some_and(|scopes| {
            scopes
                .iter()
                .any(|(k, s)| matches!(k, ScopeKey::Authority(_)) || s.egress_check(false).is_err())
        }) {
            return Err(Violation::Malformed);
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
        if outbound.is_some_and(|scopes| scopes.len() > max_authorities) {
            return Err(Violation::PushLimit);
        }
        if let Some(scopes) = outbound {
            scopes.values().try_for_each(|s| s.egress_check(true))?;
        }
        self.scopes
            .values()
            .flat_map(BTreeMap::values)
            .try_for_each(Scope::check)
    }

    /// Parses a pushed body, then checks it. Never panics, whatever the input.
    pub fn parse_checked(
        json: &str,
        max_push_endpoints: usize,
        max_authorities: usize,
    ) -> Result<Self, Violation> {
        let report: Report = serde_json::from_str(json).map_err(|e| {
            if e.is_data() && e.to_string().starts_with("unknown field") {
                Violation::UnknownField
            } else if e.is_data() && e.to_string().starts_with("invalid scope key") {
                bad_scope_key(json)
            } else {
                Violation::Malformed
            }
        })?;
        report.check(max_push_endpoints, max_authorities)?;
        Ok(report)
    }

    /// Endpoints sorted by direction, port, template, method.
    pub fn endpoints(&self) -> Vec<Endpoint<'_>> {
        let mut out = Vec::new();
        for (direction, ports) in &self.scopes {
            for (key, scope) in ports {
                let mut found = Vec::new();
                scope.root.collect(&mut Vec::new(), &mut found);
                for (path, methods) in &scope.raw {
                    found.extend(methods.iter().map(|(m, s)| Found {
                        template: path.clone(),
                        segs: Vec::new(),
                        grpc: true,
                        method: m.as_str(),
                        stats: s,
                    }));
                }
                found.sort_by(|a, b| (&a.template, a.method).cmp(&(&b.template, b.method)));
                out.extend(found.into_iter().map(|f| Endpoint {
                    direction: *direction,
                    port: key.port(),
                    authority: match key {
                        ScopeKey::Authority(authority) => Some(authority.as_str()),
                        ScopeKey::Port(_) => None,
                    },
                    method: f.method,
                    grpc: f.grpc,
                    template: f.template,
                    segs: f.segs,
                    stats: f.stats,
                }));
            }
        }
        out
    }

    /// Canonical JSON: sorted keys, `"version": 1`, or 2 when the report holds outbound scopes.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        let report: Report =
            serde_json::from_str(json).map_err(|e| format!("invalid report: {e}"))?;
        if !matches!(report.version, 1 | 2) {
            return Err(format!("unsupported report version {}", report.version));
        }
        let outbound = report.scopes.get(&Direction::Outbound);
        if (report.version == 2) != report.has_authorities() {
            return Err(format!(
                "report version {} does not match its scopes",
                report.version
            ));
        }
        if !report.counter_ok() {
            return Err("outbound exchanges out of bounds".to_string());
        }
        let inbound = report.scopes.get(&Direction::Inbound);
        if inbound.is_some_and(|s| s.keys().any(|k| !matches!(k, ScopeKey::Port(_)))) {
            return Err("inbound scope key is not a port".to_string());
        }
        if outbound.is_some_and(|s| s.keys().any(|k| !matches!(k, ScopeKey::Authority(_)))) {
            return Err("outbound scope key is not an authority".to_string());
        }
        let egress_ok = |scopes: Option<&BTreeMap<ScopeKey, Scope>>, outbound: bool| {
            scopes.is_none_or(|s| s.values().all(|scope| scope.egress_check(outbound).is_ok()))
        };
        if !egress_ok(inbound, false) || !egress_ok(outbound, true) {
            return Err("origin or tunnels out of bounds".to_string());
        }
        Ok(report)
    }
}

/// A scope key failed to parse: `authority` when an outbound key is outside the grammar of
/// §6.1 (the only place a wrong key is not a malformed report), else `malformed`.
fn bad_scope_key(json: &str) -> Violation {
    let outbound_bad = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| {
            v.pointer("/scopes/outbound")?.as_object().map(|keys| {
                keys.keys()
                    .any(|k| !matches!(ScopeKey::parse(k), Some(ScopeKey::Authority(_))))
            })
        })
        .unwrap_or(false);
    if outbound_bad {
        Violation::Authority
    } else {
        Violation::Malformed
    }
}

/// Pure merge: commutative and associative.
pub fn merge(mut a: Report, b: Report, limits: &Limits) -> Report {
    a.merge(limits, b);
    a
}
