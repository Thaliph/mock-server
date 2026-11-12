//! `MimicConfig` policy (spec §7): validation, precedence, defaults, consent and status
//! conditions. Pure: the same configs in any order give the same answers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::input::{Config, ReferenceGrant, Snapshot, TargetKind};

/// `loadBalancer.slowStart` when no `MimicConfig` sets it, in seconds.
pub const DEFAULT_SLOW_START_SECS: u32 = 30;
/// Largest `loadBalancer.slowStart`, in seconds.
pub const MAX_SLOW_START_SECS: u32 = 3600;
pub const MAX_TARGETS: usize = 16;
pub const MAX_DESTINATIONS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LoadBalancer {
    pub slow_start_seconds: u32,
}

/// The effective policy of a Service, as the view carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Policy {
    pub load_balancer: LoadBalancer,
}

/// The policy of a Service no `MimicConfig` targets.
pub fn defaults() -> Policy {
    todo!()
}

/// The effective policy of a Service from its winning `MimicConfig`, if any.
pub fn effective(winner: Option<&Config>) -> Policy {
    let _ = winner;
    todo!()
}

/// `Err(message)` when the object breaks a rule the CRD's CEL also states (MSH-08).
pub fn validate(config: &Config) -> Result<(), String> {
    let _ = config;
    todo!()
}

/// The valid `MimicConfig` that wins each target, keyed by `(kind, namespace, name)`: the
/// oldest `created`, then the lowest name (MSH-08).
pub struct Winners<'a> {
    map: BTreeMap<(TargetKind, &'a str, &'a str), &'a Config>,
}

impl<'a> Winners<'a> {
    pub fn new(configs: &'a [Config]) -> Winners<'a> {
        let _ = configs;
        let _ = &BTreeMap::<u8, u8>::new();
        todo!()
    }

    pub fn get(&self, kind: TargetKind, namespace: &str, name: &str) -> Option<&'a Config> {
        let _ = (kind, namespace, name);
        let _ = &self.map;
        todo!()
    }
}

/// Whether `dest_ns` consents to a `MimicConfig` of `from_ns` reaching its Service `name`
/// (spec §7.9); `None` grants: the `ReferenceGrant` CRD is absent.
pub fn consents(
    grants: Option<&[ReferenceGrant]>,
    from_ns: &str,
    dest_ns: &str,
    name: &str,
) -> bool {
    let _ = (grants, from_ns, dest_ns, name);
    todo!()
}

/// One status condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub ok: bool,
    pub reason: String,
    pub message: String,
}

/// `status.conditions` of a `MimicConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStatus {
    pub accepted: Condition,
    pub resolved_refs: Condition,
}

/// The status of every `MimicConfig` of the snapshot, keyed by `(namespace, name)`.
pub fn statuses(snapshot: &Snapshot) -> BTreeMap<(String, String), ConfigStatus> {
    let _ = snapshot;
    todo!()
}
