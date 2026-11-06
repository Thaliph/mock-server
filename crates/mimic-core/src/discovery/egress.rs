//! The egress set (spec §7.7): what a sidecar needs of the controller's outbound report to
//! mock its provider calls, and nothing else. Pure: no I/O, no clock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::histogram::Histogram;
use super::report::Report;
use super::sample::Sample;

/// The only format there is.
pub const FORMAT: u32 = 1;

/// Why an egress set is refused. `reason` is the fixed string logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Format,
    Authority,
    Limit,
    Malformed,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Authority => "authority",
            Self::Limit => "limit",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub grpc: bool,
    pub latency: Histogram,
    pub method: String,
    pub redacted: Vec<String>,
    pub samples: Vec<Sample>,
    pub statuses: BTreeMap<u16, u64>,
    pub template: String,
    pub volatile: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressSet {
    pub authorities: BTreeMap<String, Authority>,
    pub format: u32,
}

impl EgressSet {
    pub fn project(_report: &Report) -> Self {
        unimplemented!("egress projection")
    }

    pub fn from_json(_json: &str) -> Result<Self, Refusal> {
        unimplemented!("egress check")
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn endpoint_count(&self) -> usize {
        self.authorities.values().map(|a| a.endpoints.len()).sum()
    }
}

/// `<version>:<hex SHA-256 of outbound>`.
pub fn digest(_version: u64, _outbound: &[u8]) -> String {
    unimplemented!("digest")
}

/// The hex part of a digest field written for `version`; `None` when it is absent, malformed
/// or written for another version (stale).
pub fn fresh_hex(_digest: Option<&str>, _version: u64) -> Option<&str> {
    unimplemented!("staleness")
}

/// `"<format>-<hex>"`, quotes included.
pub fn etag(_hex: &str) -> String {
    unimplemented!("etag")
}

/// An `If-None-Match` list that holds `etag` as one of its entries, byte for byte.
pub fn matches_etag(_if_none_match: &str, _etag: &str) -> bool {
    unimplemented!("if-none-match")
}

/// The local view of a snapshot: `(set, etag, pulled)`, or the empty view.
pub fn summary(_snapshot: Option<(&EgressSet, &str, &str)>) -> String {
    unimplemented!("summary")
}
