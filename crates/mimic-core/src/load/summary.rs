//! Results of a run (spec §7.8 "Results"): per endpoint and in total.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::histogram::Histogram;
use super::plan::Plan;
use crate::mesh::reason::Reason;

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

/// A key of `mimicErrors`: one of [`Reason::trusted_inbound`], ordered and written as its
/// `x-mimic-error` value, so that a map reads `connect-failed`, `overloaded`, `queue-timeout`
/// (spec §7.8 "Results").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MimicError(Reason);

impl MimicError {
    /// The key of a trusted inbound refusal; none for any other reason.
    pub fn new(reason: Reason) -> Option<MimicError> {
        if Reason::trusted_inbound().contains(&reason) {
            Some(MimicError(reason))
        } else {
            None
        }
    }

    pub const fn reason(self) -> Reason {
        self.0
    }

    pub const fn as_str(self) -> &'static str {
        self.0.as_str()
    }
}

impl Ord for MimicError {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for MimicError {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Serialize for MimicError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MimicError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<MimicError, D::Error> {
        struct Key;
        impl de::Visitor<'_> for Key {
            type Value = MimicError;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mimicErrors key")
            }

            // The message never quotes the value: it comes from a post.
            fn visit_str<E: de::Error>(self, v: &str) -> Result<MimicError, E> {
                Reason::parse(v.as_bytes())
                    .and_then(MimicError::new)
                    .ok_or_else(|| E::custom("unknown mimicErrors key"))
            }
        }
        deserializer.deserialize_str(Key)
    }
}

fn bump<K: Ord>(map: &mut BTreeMap<K, u64>, key: K, n: u64) {
    let c = map.entry(key).or_default();
    *c = c.saturating_add(n);
}

/// Cumulative counts of one endpoint, on one pod or merged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub requests: u64,
    pub missed: u64,
    pub statuses: BTreeMap<Class, u64>,
    pub errors: BTreeMap<ErrorKind, u64>,
    /// Trusted inbound refusals, by reason; left out when there is none (an F11 document).
    #[serde(
        rename = "mimicErrors",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub mimic_errors: BTreeMap<MimicError, u64>,
    pub latency: Histogram,
}

impl Stats {
    /// A response head arrived `micros` after the first byte was written.
    pub fn response(&mut self, status: u16, micros: u64) {
        let class = match status {
            200..=299 => Class::C2xx,
            300..=399 => Class::C3xx,
            400..=499 => Class::C4xx,
            500..=599 => Class::C5xx,
            // Not a final status of HTTP/1.1.
            _ => return self.error(ErrorKind::Protocol),
        };
        self.requests = self.requests.saturating_add(1);
        bump(&mut self.statuses, class, 1);
        self.latency.record(micros);
    }

    /// A request that got no usable response.
    pub fn error(&mut self, kind: ErrorKind) {
        self.requests = self.requests.saturating_add(1);
        bump(&mut self.errors, kind, 1);
    }

    /// A trusted refusal of the target's inbound admission: a request, counted under its
    /// reason only (no status, no latency).
    pub fn refusal(&mut self, error: MimicError) {
        self.requests = self.requests.saturating_add(1);
        bump(&mut self.mimic_errors, error, 1);
    }

    /// Sends that were due with the concurrency in flight (not requests).
    pub fn missed(&mut self, n: u64) {
        self.missed = self.missed.saturating_add(n);
    }

    pub fn merge(&mut self, other: &Stats) {
        self.requests = self.requests.saturating_add(other.requests);
        self.missed = self.missed.saturating_add(other.missed);
        for (class, n) in &other.statuses {
            bump(&mut self.statuses, *class, *n);
        }
        for (kind, n) in &other.errors {
            bump(&mut self.errors, *kind, *n);
        }
        for (error, n) in &other.mimic_errors {
            bump(&mut self.mimic_errors, *error, *n);
        }
        self.latency.merge(&other.latency);
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
        if self.endpoints.len() < other.endpoints.len() {
            self.endpoints
                .resize_with(other.endpoints.len(), Stats::default);
        }
        for (mine, theirs) in self.endpoints.iter_mut().zip(&other.endpoints) {
            mine.merge(theirs);
        }
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mimic_errors: BTreeMap<MimicError, u64>,
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
    let mut merged = PodResults {
        endpoints: vec![Stats::default(); plan.endpoints.len()],
    };
    for pod in pods {
        merged.merge(pod);
    }
    let mut total = Stats::default();
    let mut endpoints = Vec::with_capacity(plan.endpoints.len());
    for (endpoint, stats) in plan.endpoints.iter().zip(&merged.endpoints) {
        total.merge(stats);
        endpoints.push(EndpointRow {
            method: endpoint.method.clone(),
            port: endpoint.port,
            template: endpoint.template.clone(),
            row: row(stats, elapsed_ms),
        });
    }
    Summary {
        total: row(&total, elapsed_ms),
        endpoints,
    }
}

fn row(stats: &Stats, elapsed_ms: u64) -> Row {
    let rps = if elapsed_ms == 0 {
        0
    } else {
        let n = u128::from(stats.requests) * 1000 + u128::from(elapsed_ms / 2);
        u64::try_from(n / u128::from(elapsed_ms)).unwrap_or(u64::MAX)
    };
    Row {
        requests: stats.requests,
        missed: stats.missed,
        statuses: stats.statuses.clone(),
        errors: stats.errors.clone(),
        mimic_errors: stats.mimic_errors.clone(),
        rps,
        latency_micros: Latency {
            p50: stats.latency.percentile(50),
            p90: stats.latency.percentile(90),
            p99: stats.latency.percentile(99),
            max: stats.latency.max(),
        },
    }
}
