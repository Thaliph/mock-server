//! The `status.discovery` of a `MimicWorkload` (spec §7.4): a pure function of the deployment's
//! `:meta`, its pods' last push times, the window and the current time.

use std::collections::BTreeMap;

use serde_json::Value;

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
    let _ = (meta, pods, window_ms, now_ms);
    todo!("status")
}

/// `ms` as RFC 3339 in UTC, to the second.
pub fn rfc3339_seconds(ms: u64) -> String {
    let _ = ms;
    todo!("rfc3339")
}
