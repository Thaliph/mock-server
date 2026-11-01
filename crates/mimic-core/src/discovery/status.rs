//! The `status.discovery` of a `MimicWorkload` (spec §7.4): a pure function of the deployment's
//! `:meta`, its pods' last push times, the window and the current time.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// What `:meta` holds of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub inbound: u64,
    pub outbound: u64,
    pub exchanges: u64,
    pub sample_bytes: u64,
    /// Last applied push, Unix milliseconds.
    pub updated_ms: u64,
}

/// `status.discovery`. `pods` maps a pod to its last push (Unix milliseconds); a pod counts when
/// that is at or after `now_ms - window_ms`. Without `meta` the counts are zero.
pub fn discovery(
    meta: Option<&Meta>,
    pods: &BTreeMap<String, u64>,
    window_ms: u64,
    now_ms: u64,
) -> Value {
    let Some(meta) = meta else {
        return json!({"endpoints": {}, "exchanges": 0, "sampleBytes": 0, "pods": 0});
    };
    let floor = now_ms.saturating_sub(window_ms);
    let counted = pods.values().filter(|last| **last >= floor).count();
    let mut endpoints = Map::new();
    for (direction, count) in [("inbound", meta.inbound), ("outbound", meta.outbound)] {
        if count > 0 {
            endpoints.insert(direction.into(), json!(count));
        }
    }
    json!({
        "endpoints": endpoints,
        "exchanges": meta.exchanges,
        "sampleBytes": meta.sample_bytes,
        "pods": counted,
        "lastPushTime": rfc3339_seconds(meta.updated_ms),
    })
}

/// `ms` as RFC 3339 in UTC, to the second.
pub fn rfc3339_seconds(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (proleptic Gregorian, years from 1 March).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
