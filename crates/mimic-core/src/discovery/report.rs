//! The local report (spec §6): endpoints keyed by direction, port, method and path template,
//! with status counters, a latency histogram and the hosts seen. Merging is pure,
//! commutative and associative; recording an exchange merges a one-exchange report.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::histogram::Histogram;

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

impl Report {
    pub fn new(_pod: Option<String>) -> Self {
        todo!()
    }

    pub fn exchanges(&self) -> u64 {
        self.exchanges
    }

    /// Records one exchange: the same as merging a one-exchange report.
    pub fn record(&mut self, _limits: &Limits, _exchange: &Exchange<'_>) {
        todo!()
    }

    /// Merges `other` into `self`.
    pub fn merge(&mut self, _limits: &Limits, _other: Report) {
        todo!()
    }

    /// Endpoints sorted by direction, port, template, method.
    pub fn endpoints(&self) -> Vec<Endpoint<'_>> {
        todo!()
    }

    /// Canonical JSON: sorted keys, `"version": 1`.
    pub fn to_json(&self) -> String {
        todo!()
    }

    pub fn from_json(_json: &str) -> Result<Self, String> {
        todo!()
    }
}

/// Pure merge: commutative and associative.
pub fn merge(mut a: Report, b: Report, limits: &Limits) -> Report {
    a.merge(limits, b);
    a
}
