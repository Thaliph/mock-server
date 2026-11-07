//! Results of a run (spec §7.8 "Results"): per endpoint and in total.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::histogram::Histogram;
use super::plan::Plan;

/// Status class of a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Class {
    #[serde(rename = "2xx")]
    C2xx,
    #[serde(rename = "3xx")]
    C3xx,
    #[serde(rename = "4xx")]
    C4xx,
    #[serde(rename = "5xx")]
    C5xx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    Timeout,
    Connect,
    Reset,
    Protocol,
    Refused,
    Unverified,
}

/// Cumulative counts of one endpoint, on one pod or merged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub requests: u64,
    pub missed: u64,
    pub statuses: BTreeMap<Class, u64>,
    pub errors: BTreeMap<ErrorKind, u64>,
    pub latency: Histogram,
}

impl Stats {
    /// A response head arrived `micros` after the first byte was written.
    pub fn response(&mut self, status: u16, micros: u64) {
        todo!()
    }

    pub fn error(&mut self, kind: ErrorKind) {
        todo!()
    }

    pub fn missed(&mut self, n: u64) {
        todo!()
    }

    pub fn merge(&mut self, other: &Stats) {
        todo!()
    }
}

/// The cumulative results of one generator pod: one `Stats` per plan endpoint, in plan order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodResults {
    pub endpoints: Vec<Stats>,
}

impl PodResults {
    pub fn merge(&mut self, other: &PodResults) {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latency {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub requests: u64,
    pub missed: u64,
    pub statuses: BTreeMap<Class, u64>,
    pub errors: BTreeMap<ErrorKind, u64>,
    pub rps: u64,
    pub latency_micros: Latency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRow {
    pub method: String,
    pub port: u16,
    pub template: String,
    #[serde(flatten)]
    pub row: Row,
}

/// `status.results`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub total: Row,
    pub endpoints: Vec<EndpointRow>,
}

/// The summary of the pods' cumulative results after `elapsed_ms`.
pub fn summary(plan: &Plan, pods: &[PodResults], elapsed_ms: u64) -> Summary {
    todo!()
}
