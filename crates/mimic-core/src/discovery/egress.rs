//! The egress set (spec §7.7): what a sidecar needs of the controller's outbound report to
//! mock its provider calls, and nothing else. Pure: no I/O, no clock.

use crate::compact::{Name, SortedMap, SortedSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::check::Violation;
use super::histogram::Histogram;
use super::redact::{MARKER, Redactor};
use super::report::{Direction, MAX_PATH, MAX_SEGMENTS, Report};
use super::sample::{MAX_SAMPLES, Sample};
use super::scope::ScopeKey;
use super::template::split_path;
use crate::Mode;
use crate::mock::factor_number;

/// The first format: the samples of a sidecar up to F13.
pub const FORMAT_1: u32 = 1;
/// The format of F14: format 1 plus the gRPC parts of the samples (spec §6.4, §7.7).
pub const FORMAT: u32 = 2;
/// Most authorities in a set.
pub const MAX_AUTHORITIES: usize = 10_000;
/// Most endpoints in a set, all authorities together.
pub const MAX_ENDPOINTS: usize = 10_000;
/// Most field names of an endpoint, `volatile` and `redacted` together.
pub const MAX_NAMES: usize = 128;
/// Longest field name, in bytes.
pub const MAX_NAME: usize = 128;

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

/// One endpoint of an authority. Fields in byte order of their names: the JSON is canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub grpc: bool,
    pub latency: Histogram,
    pub method: String,
    pub redacted: Vec<String>,
    pub samples: Vec<Sample>,
    pub statuses: SortedMap<u16, u64>,
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
    pub authorities: SortedMap<Name, Authority>,
    pub format: u32,
}

impl Endpoint {
    /// The bounds of spec §7.3 that apply to an egress endpoint.
    fn check(&self) -> Result<(), Refusal> {
        let names = self.volatile.iter().chain(&self.redacted);
        let ok = self.template.len() <= MAX_PATH
            && split_path(&self.template).len() <= MAX_SEGMENTS
            && self.statuses.keys().all(|s| (100..=599).contains(s))
            && self.latency.check().is_ok()
            && self.samples.len() <= MAX_SAMPLES
            // A stamp is the controller's, never part of a projection.
            && self
                .samples
                .iter()
                .all(|s| s.last_seen().is_none() && s.check().is_ok())
            && self.volatile.len() + self.redacted.len() <= MAX_NAMES
            && names.into_iter().all(|n| n.len() <= MAX_NAME);
        if ok { Ok(()) } else { Err(Refusal::Malformed) }
    }
}

/// Only to read `format` before the strict parse, so that a set of another format is told
/// from a malformed one.
#[derive(Deserialize)]
struct Head {
    format: Option<Value>,
}

impl EgressSet {
    /// The projection of a report (spec §7.7): the outbound scopes that hold an endpoint and,
    /// per endpoint, what a mock needs. A pure function of the outbound part.
    pub fn project(report: &Report) -> Self {
        Self::project_as(report, FORMAT)
    }

    /// As [`EgressSet::project`], in `format`: format 1 leaves the gRPC parts out of the samples.
    pub fn project_as(report: &Report, format: u32) -> Self {
        let mut authorities: SortedMap<Name, Authority> = SortedMap::new();
        for e in report.endpoints() {
            let Some(authority) = e.authority.filter(|_| e.direction == Direction::Outbound) else {
                continue;
            };
            let fields = e.stats.observed().volatility();
            let names = |keep: &dyn Fn(&str) -> bool| -> Vec<String> {
                fields
                    .fields()
                    .into_iter()
                    .filter(|f| keep(f))
                    .map(str::to_string)
                    .collect()
            };
            authorities
                .entry(Name::from(authority))
                .or_insert_with(|| Authority {
                    endpoints: Vec::new(),
                })
                .endpoints
                .push(Endpoint {
                    grpc: e.grpc,
                    latency: e.stats.latency.clone(),
                    method: e.method.to_string(),
                    redacted: names(&|f| fields.is_redacted(f)),
                    samples: e
                        .stats
                        .samples()
                        .iter()
                        .map(|(_, s)| {
                            if format == FORMAT_1 && s.is_grpc() {
                                s.without_grpc().1.unstamped()
                            } else {
                                s.unstamped()
                            }
                        })
                        .collect(),
                    statuses: e.stats.statuses.clone(),
                    template: e.template,
                    volatile: names(&|f| fields.is_volatile(f)),
                });
        }
        // The upgraded form, so that every set is canonical: it reads back byte for byte.
        EgressSet {
            authorities,
            format,
        }
        .upgraded()
    }

    /// Reads a set and checks it (spec §7.7). Never panics, whatever the input.
    pub fn from_json(json: &str) -> Result<Self, Refusal> {
        let head: Head = serde_json::from_str(json).map_err(|_| Refusal::Malformed)?;
        let format = head.format.and_then(|f| f.as_u64());
        if !matches!(format, Some(1 | 2)) {
            return Err(Refusal::Format);
        }
        let set: EgressSet = serde_json::from_str(json).map_err(|_| Refusal::Malformed)?;
        if set.authorities.len() > MAX_AUTHORITIES {
            return Err(Refusal::Limit);
        }
        if !set
            .authorities
            .keys()
            .all(|k| matches!(ScopeKey::parse(k), Some(ScopeKey::Authority(_))))
        {
            return Err(Refusal::Authority);
        }
        if set.endpoint_count() > MAX_ENDPOINTS {
            return Err(Refusal::Limit);
        }
        // Format 1 is what a sidecar up to F13 reads: no gRPC part.
        if set.format == FORMAT_1
            && set
                .authorities
                .values()
                .flat_map(|a| &a.endpoints)
                .flat_map(|e| &e.samples)
                .any(Sample::is_grpc)
        {
            return Err(Refusal::Malformed);
        }
        for authority in set.authorities.values() {
            // A mock picks an endpoint by method and template: twice would be ambiguous.
            let mut seen = SortedSet::new();
            for endpoint in &authority.endpoints {
                endpoint.check()?;
                if !seen.insert((endpoint.method.as_str(), endpoint.template.as_str())) {
                    return Err(Refusal::Malformed);
                }
            }
        }
        Ok(set.upgraded())
    }

    /// The placeholder upgrade (spec §6) of an egress set written up to F12: a sample that
    /// holds the legacy marker is redacted again, as an outbound request and its response, and
    /// samples that become equal are one. A set without the marker is returned as it is.
    pub fn upgraded(mut self) -> Self {
        let request = Redactor::default().outbound_request();
        let response = Redactor::default();
        for endpoint in self
            .authorities
            .values_mut()
            .flat_map(|a| a.endpoints.iter_mut())
        {
            let legacy = |s: &Sample| serde_json::to_string(s).is_ok_and(|t| t.contains(MARKER));
            if !endpoint.samples.iter().any(legacy) {
                continue;
            }
            let mut keyed: Vec<(u64, Sample)> = Vec::new();
            for mut sample in std::mem::take(&mut endpoint.samples) {
                if legacy(&sample) {
                    sample.redact(&request, &response);
                    // A body that grows past the limit takes its sample with it.
                    if sample.check().is_err_and(|v| v == Violation::BodySize) {
                        continue;
                    }
                }
                keyed.push((sample.reseal(), sample));
            }
            keyed.sort_by_key(|(hash, _)| *hash);
            keyed.dedup_by_key(|(hash, _)| *hash);
            endpoint.samples = keyed.into_iter().map(|(_, s)| s).collect();
        }
        self
    }

    /// Canonical JSON: sorted keys, so equal sets give equal bytes.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn endpoint_count(&self) -> usize {
        self.authorities.values().map(|a| a.endpoints.len()).sum()
    }
}

/// `<version>:<hex SHA-256 of outbound>`: the `:meta` field `outbound_digest` (spec §7.7).
/// Pass no bytes for a deployment without `:outbound`.
pub fn digest(version: u64, outbound: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = Sha256::digest(outbound);
    let mut out = String::with_capacity(20 + 64);
    out.push_str(&version.to_string());
    out.push(':');
    for byte in hash.iter() {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
    out
}

/// The hex part of a digest field written for `version`; `None` when it is absent, malformed
/// or written for another version, which is *stale*: the reader then hashes the bytes itself.
pub fn fresh_hex(digest: Option<&str>, version: u64) -> Option<&str> {
    let (written, hex) = digest?.split_once(':')?;
    let well_formed =
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (well_formed && written == version.to_string()).then_some(hex)
}

/// The digest of a format 2 answer (spec §7.7): the outbound hex alone without a `:grpc`, else
/// the SHA-256 of `<outbound hex>:<grpc hex>`, so that a change of either changes the `ETag`.
pub fn composite_hex(outbound_hex: &str, grpc_hex: Option<&str>) -> String {
    let Some(grpc) = grpc_hex else {
        return outbound_hex.to_string();
    };
    let hash = Sha256::digest(format!("{outbound_hex}:{grpc}").as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// `"<format>-<hex>"`, quotes included.
pub fn etag(format: u32, hex: &str) -> String {
    format!("\"{format}-{hex}\"")
}

/// An `If-None-Match` list that holds `etag` as one of its entries, byte for byte: weak
/// validators and `*` never match.
pub fn matches_etag(if_none_match: &str, etag: &str) -> bool {
    if_none_match
        .split(',')
        .any(|entry| entry.trim_matches([' ', '\t']) == etag)
}

#[derive(Serialize)]
pub(crate) struct ViewEndpoint<'a> {
    pub(crate) exchanges: u64,
    pub(crate) grpc: bool,
    pub(crate) method: &'a str,
    pub(crate) statuses: &'a SortedMap<u16, u64>,
    pub(crate) template: &'a str,
}

#[derive(Serialize)]
pub(crate) struct ViewAuthority<'a> {
    pub(crate) endpoints: Vec<ViewEndpoint<'a>>,
}

#[derive(Serialize)]
struct View<'a> {
    authorities: SortedMap<&'a str, ViewAuthority<'a>>,
    etag: Option<&'a str>,
    latency_factor: serde_json::Number,
    mode: &'static str,
    pulled: Option<&'a str>,
}

/// The local view of a snapshot given as its authorities, with its `ETag` and pull time.
pub(crate) fn render_summary<'a>(
    authorities: SortedMap<&'a str, ViewAuthority<'a>>,
    etag: &'a str,
    pulled: &'a str,
    mode: Mode,
    factor: u32,
) -> String {
    let view = View {
        authorities,
        etag: Some(etag),
        latency_factor: factor_number(factor),
        mode: mode.as_str(),
        pulled: Some(pulled),
    };
    // Plain data: serialisation cannot fail.
    serde_json::to_string(&view).unwrap_or_default()
}

/// The local view of a snapshot (`(set, etag, pulled)`), or of none: what the pod could mock,
/// with no sample and no field name, and the mode and latency factor (hundredths) in force.
/// Sorted keys, so equal input gives equal bytes.
pub fn summary(snapshot: Option<(&EgressSet, &str, &str)>, mode: Mode, factor: u32) -> String {
    let latency_factor = factor_number(factor);
    let view = match snapshot {
        None => View {
            authorities: SortedMap::new(),
            etag: None,
            latency_factor,
            mode: mode.as_str(),
            pulled: None,
        },
        Some((set, etag, pulled)) => View {
            authorities: set
                .authorities
                .iter()
                .map(|(name, a)| {
                    let endpoints = a
                        .endpoints
                        .iter()
                        .map(|e| ViewEndpoint {
                            exchanges: e.statuses.values().fold(0, |n, c| n.saturating_add(*c)),
                            grpc: e.grpc,
                            method: &e.method,
                            statuses: &e.statuses,
                            template: &e.template,
                        })
                        .collect();
                    (name.as_str(), ViewAuthority { endpoints })
                })
                .collect(),
            etag: Some(etag),
            latency_factor,
            mode: mode.as_str(),
            pulled: Some(pulled),
        },
    };
    // Plain data: serialisation cannot fail.
    serde_json::to_string(&view).unwrap_or_default()
}
