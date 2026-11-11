//! The load plan (spec §7.8 "Plan"): which inbound endpoints are loaded, with which samples.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discovery::placeholder::is_marked;
use crate::discovery::redact::MARKER;
use crate::discovery::report::{Direction, Report};
use crate::discovery::sample::{self as recorded, Payload};

/// Smallest `MIMIC_LOAD_MAX_PLAN` (spec §10.2): the skeleton of an empty plan is about 34
/// bytes, so a plan shrunk to this limit is always readable.
pub const MIN_PLAN: usize = 64 << 10;

/// Largest `MIMIC_LOAD_MAX_PLAN`.
pub const MAX_PLAN: usize = 256 << 20;

/// Parses `MIMIC_LOAD_MAX_PLAN` (`<n>`, `<n>Ki`, `<n>Mi`; 64Ki–256Mi).
pub fn plan_limit(value: &str) -> Result<usize, String> {
    let (digits, unit) = match value {
        v if v.ends_with("Ki") => (&v[..v.len() - 2], 1usize << 10),
        v if v.ends_with("Mi") => (&v[..v.len() - 2], 1usize << 20),
        v => (v, 1),
    };
    let bytes = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(|| format!("invalid size {value:?}, expected <n>, <n>Ki or <n>Mi"))?;
    if (MIN_PLAN..=MAX_PLAN).contains(&bytes) {
        Ok(bytes)
    } else {
        Err(format!(
            "size {value:?} out of range, expected 64Ki to 256Mi"
        ))
    }
}

/// A credential the test supplies: the request field it replaces and the Secret key that holds
/// the value. The plan holds these two names, never a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialField {
    pub field: String,
    pub key: String,
}

/// What of the `MimicLoadTest` spec and of the controller's limits the plan depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// `endpoints.include`: `<METHOD> <template>` entries; `None` keeps every endpoint.
    pub include: Option<BTreeSet<String>>,
    /// `endpoints.exclude`, which wins over `include`.
    pub exclude: BTreeSet<String>,
    pub credentials: Vec<CredentialField>,
    /// `MIMIC_LOAD_MAX_ENDPOINTS`.
    pub max_endpoints: usize,
    /// `MIMIC_LOAD_MAX_PLAN`, in bytes of the serialised plan; `plan` raises a value below
    /// `MIN_PLAN` (see `plan_limit`) to it, else a plan shrunk to the limit may be unreadable.
    pub max_plan: usize,
}

/// A request body as the sample holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum Body {
    /// The canonical text of a JSON value.
    Json { media: String, text: String },
    /// The pairs of a urlencoded form, in order.
    Form {
        media: String,
        pairs: Vec<(String, String)>,
    },
}

/// One redacted request of an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub hash: u64,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub method: String,
    pub port: u16,
    pub template: String,
    /// The first host of the endpoint in byte order.
    pub host: Option<String>,
    /// Exchanges recorded: the weight of the endpoint in a draw.
    pub weight: u64,
    /// The recorder's volatile request fields (`query/…`, `request.header/…`, `request.body/…`).
    pub volatile: Vec<String>,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub credentials: Vec<CredentialField>,
    pub endpoints: Vec<Endpoint>,
}

/// A plan and why the other endpoints are not in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Planned {
    pub plan: Plan,
    /// Endpoints left out, by reason (`status.skipped`).
    pub skipped: BTreeMap<&'static str, u64>,
}

// Why an endpoint is left out: the keys of `status.skipped`.
const GRPC: &str = "grpc";
const UPGRADE: &str = "upgrade";
const EXCLUDED: &str = "excluded";
const NO_SAMPLE: &str = "no_sample";
const CREDENTIALS: &str = "credentials";
const OPAQUE_BODY: &str = "opaque_body";
const LIMIT: &str = "limit";
const PLAN_SIZE: &str = "plan_size";

/// Escapes a JSON object key as a pointer token.
fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The credential fields of the leaves of a JSON value that hold the marker.
/// Returns `false` when an object key holds the marker: a name cannot be supplied.
fn marked_leaves(pointer: &str, value: &Value, out: &mut BTreeSet<String>) -> bool {
    match value {
        Value::String(s) if is_marked(s) => {
            out.insert(format!("body{pointer}"));
        }
        Value::Object(map) => {
            for (k, v) in map {
                if is_marked(k) || !marked_leaves(&format!("{pointer}/{}", token(k)), v, out) {
                    return false;
                }
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                if !marked_leaves(&format!("{pointer}/{i}"), v, out) {
                    return false;
                }
            }
        }
        _ => {}
    }
    true
}

/// The credential fields a sample needs, or `None` when it needs one that cannot be supplied
/// (a redacted path segment, header name, query key, form key or JSON object key).
fn needs(sample: &recorded::Sample) -> Option<BTreeSet<String>> {
    if is_marked(sample.path()) {
        return None;
    }
    let mut out = BTreeSet::new();
    for (name, value) in sample.request_headers() {
        if is_marked(name) {
            return None;
        }
        if is_marked(value) {
            out.insert(format!("header/{name}"));
        }
    }
    for (key, value) in sample.query() {
        if is_marked(key) {
            return None;
        }
        if is_marked(value) {
            out.insert(format!("query/{key}"));
        }
    }
    match sample.request_payload() {
        Some((_, Payload::Json(text))) => {
            let value = serde_json::from_str(text).unwrap_or(Value::Null);
            if !marked_leaves("", &value, &mut out) {
                return None;
            }
        }
        Some((_, Payload::Form(pairs))) => {
            for (key, value) in pairs {
                if is_marked(&key) {
                    return None;
                }
                if is_marked(&value) {
                    out.insert(format!("body/{key}"));
                }
            }
        }
        None => {}
    }
    Some(out)
}

/// A request that had a body the report did not keep: a `content-type` without a body.
fn opaque_body(sample: &recorded::Sample) -> bool {
    sample.request_payload().is_none()
        && sample
            .request_headers()
            .iter()
            .any(|(name, _)| name == "content-type")
}

fn plan_sample(hash: u64, sample: &recorded::Sample) -> Sample {
    let body = sample
        .request_payload()
        .map(|(media, payload)| match payload {
            Payload::Json(text) => Body::Json {
                media: media.to_string(),
                text: text.to_string(),
            },
            Payload::Form(pairs) => Body::Form {
                media: media.to_string(),
                pairs,
            },
        });
    Sample {
        hash,
        path: sample.path().to_string(),
        query: sample.query().to_vec(),
        headers: sample.request_headers().to_vec(),
        body,
    }
}

/// A host the `host` header can carry.
fn host_ok(host: &str) -> bool {
    !host.is_empty() && !host.contains(MARKER) && host.bytes().all(|b| b.is_ascii_graphic())
}

/// The plan of a stored inbound report.
pub fn plan(report: &Report, spec: &Spec) -> Planned {
    build(report, spec, spec.max_plan.max(MIN_PLAN))
}

/// `plan` with an explicit byte limit, not raised to `MIN_PLAN` (tests).
pub(super) fn build(report: &Report, spec: &Spec, max_plan: usize) -> Planned {
    let supplied: BTreeSet<&str> = spec.credentials.iter().map(|c| c.field.as_str()).collect();
    let mut skipped: BTreeMap<&'static str, u64> = BTreeMap::new();
    // Usable endpoints in report order.
    let mut usable: Vec<Endpoint> = Vec::new();
    for e in report.endpoints() {
        if e.direction != Direction::Inbound {
            continue;
        }
        let key = format!("{} {}", e.method, e.template);
        if e.grpc {
            *skipped.entry(GRPC).or_default() += 1;
            continue;
        }
        if e.stats.statuses.contains_key(&101) {
            *skipped.entry(UPGRADE).or_default() += 1;
            continue;
        }
        let listed = spec.include.as_ref().is_none_or(|i| i.contains(&key));
        if !listed || spec.exclude.contains(&key) {
            *skipped.entry(EXCLUDED).or_default() += 1;
            continue;
        }
        let all: Vec<(u64, &recorded::Sample)> = e.stats.samples().iter().collect();
        let success = |s: &recorded::Sample| (200..400).contains(&s.status());
        let by_class: Vec<_> = if all.iter().any(|(_, s)| success(s)) {
            all.into_iter().filter(|(_, s)| success(s)).collect()
        } else {
            all.into_iter()
                .filter(|(_, s)| (400..600).contains(&s.status()))
                .collect()
        };
        if by_class.is_empty() {
            *skipped.entry(NO_SAMPLE).or_default() += 1;
            continue;
        }
        let mut samples = Vec::new();
        let mut credential_refused = false;
        for (hash, sample) in by_class {
            if opaque_body(sample) {
                continue;
            }
            match needs(sample) {
                Some(n) if n.iter().all(|f| supplied.contains(f.as_str())) => {
                    samples.push(plan_sample(hash, sample));
                }
                _ => credential_refused = true,
            }
        }
        if samples.is_empty() {
            let reason = if credential_refused {
                CREDENTIALS
            } else {
                OPAQUE_BODY
            };
            *skipped.entry(reason).or_default() += 1;
            continue;
        }
        let fields = e.stats.observed().volatility();
        usable.push(Endpoint {
            method: e.method.to_string(),
            port: e.port,
            template: e.template.clone(),
            host: e
                .stats
                .hosts
                .iter()
                .map(|h| h.as_str())
                .find(|h| host_ok(h))
                .map(str::to_string),
            weight: e
                .stats
                .statuses
                .values()
                .fold(0u64, |n, c| n.saturating_add(*c))
                .max(1),
            volatile: fields
                .fields()
                .into_iter()
                .filter(|f| {
                    fields.is_volatile(f)
                        && !fields.is_redacted(f)
                        && ["query/", "request.header/", "request.body/"]
                            .iter()
                            .any(|p| f.starts_with(p))
                })
                .map(str::to_string)
                .collect(),
            samples,
        });
    }
    // The endpoints with the most exchanges, ties in key (report) order.
    if usable.len() > spec.max_endpoints {
        let mut order: Vec<usize> = (0..usable.len()).collect();
        order.sort_by(|a, b| usable[*b].weight.cmp(&usable[*a].weight).then(a.cmp(b)));
        let keep: BTreeSet<usize> = order.into_iter().take(spec.max_endpoints).collect();
        *skipped.entry(LIMIT).or_default() += (usable.len() - keep.len()) as u64;
        usable = usable
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, e)| e)
            .collect();
    }
    let mut plan = Plan {
        credentials: spec.credentials.clone(),
        endpoints: usable,
    };
    let emptied = shrink(&mut plan, max_plan);
    if emptied > 0 {
        *skipped.entry(PLAN_SIZE).or_default() += emptied;
    }
    Planned { plan, skipped }
}

fn size(plan: &Plan) -> usize {
    serde_json::to_vec(plan).map_or(usize::MAX, |v| v.len())
}

/// Drops the sample with the highest content hash until the plan fits. Returns how many
/// endpoints were left without sample, and removed.
fn shrink(plan: &mut Plan, max: usize) -> u64 {
    let mut emptied = 0;
    loop {
        let excess = size(plan).saturating_sub(max);
        if excess == 0 {
            return emptied;
        }
        let mut hashes: Vec<(u64, usize)> = plan
            .endpoints
            .iter()
            .enumerate()
            .flat_map(|(i, e)| e.samples.iter().map(move |s| (s.hash, i)))
            .collect();
        hashes.sort_unstable();
        if hashes.is_empty() {
            // Only the skeleton is left, and it does not fit.
            plan.credentials.clear();
            return emptied;
        }
        // Drop at least the excess in one round, then measure again.
        let mut freed = 0usize;
        while freed < excess {
            let Some((hash, i)) = hashes.pop() else { break };
            let samples = &mut plan.endpoints[i].samples;
            if let Some(at) = samples.iter().position(|s| s.hash == hash) {
                freed += serde_json::to_vec(&samples[at]).map_or(1, |v| v.len() + 1);
                samples.remove(at);
            }
        }
        let before = plan.endpoints.len();
        plan.endpoints.retain(|e| !e.samples.is_empty());
        emptied += (before - plan.endpoints.len()) as u64;
    }
}

impl Plan {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Reads a plan of at most `max_plan` bytes.
    pub fn from_json(json: &str, max_plan: usize) -> Result<Self, String> {
        if json.len() > max_plan {
            return Err(format!("plan over {max_plan} bytes"));
        }
        serde_json::from_str(json).map_err(|e| format!("invalid plan: {e}"))
    }
}
