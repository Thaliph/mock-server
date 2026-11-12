//! `MimicConfig` policy (spec §7): validation, precedence, defaults, consent and status
//! conditions. Pure: the same configs in any order give the same answers.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::input::{Config, MIMIC_GROUP, ReferenceGrant, Snapshot, TargetKind};

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
    Policy {
        load_balancer: LoadBalancer {
            slow_start_seconds: DEFAULT_SLOW_START_SECS,
        },
    }
}

/// The effective policy of a Service from its winning `MimicConfig`, if any.
pub fn effective(winner: Option<&Config>) -> Policy {
    Policy {
        load_balancer: LoadBalancer {
            slow_start_seconds: winner
                .and_then(|c| c.slow_start)
                .unwrap_or(DEFAULT_SLOW_START_SECS),
        },
    }
}

/// `Err(message)` when the object breaks a rule the CRD's CEL also states (MSH-08).
pub fn validate(config: &Config) -> Result<(), String> {
    let has = |kind| config.targets.iter().any(|t| t.kind == kind);
    if config.targets.is_empty() || config.targets.len() > MAX_TARGETS {
        return Err(format!("targetRefs must hold 1 to {MAX_TARGETS} entries"));
    }
    if config.targets.iter().any(|t| t.name.is_empty()) {
        return Err("a targetRef has no name".into());
    }
    if let Some(destinations) = &config.destinations {
        if !has(TargetKind::Deployment) {
            return Err("destinations fit a Deployment target only".into());
        }
        if destinations.len() > MAX_DESTINATIONS {
            return Err(format!(
                "destinations holds more than {MAX_DESTINATIONS} entries"
            ));
        }
        if destinations
            .iter()
            .any(|d| d.namespace.is_empty() || d.name.is_empty())
        {
            return Err("a destination needs a namespace and a name".into());
        }
    }
    if config.mesh_enabled.is_some() && !has(TargetKind::Deployment) {
        return Err("mesh fits a Deployment target only".into());
    }
    if let Some(secs) = config.slow_start {
        if !has(TargetKind::Service) {
            return Err("loadBalancer fits a Service target only".into());
        }
        if secs > MAX_SLOW_START_SECS {
            return Err(format!("slowStart is over {MAX_SLOW_START_SECS}s"));
        }
    }
    Ok(())
}

type TargetKey<'a> = (TargetKind, &'a str, &'a str);

/// The valid `MimicConfig` that wins each target, keyed by `(kind, namespace, name)`: the
/// oldest `created`, then the lowest name (MSH-08).
pub struct Winners<'a> {
    map: BTreeMap<TargetKey<'a>, &'a Config>,
}

impl<'a> Winners<'a> {
    pub fn new(configs: &'a [Config]) -> Winners<'a> {
        let mut map: BTreeMap<TargetKey<'a>, &'a Config> = BTreeMap::new();
        for config in configs.iter().filter(|c| validate(c).is_ok()) {
            for target in &config.targets {
                let key = (target.kind, config.namespace.as_str(), target.name.as_str());
                let rank = |c: &Config| (c.created, c.name.clone());
                match map.get(&key) {
                    Some(held) if rank(held) <= rank(config) => {}
                    _ => {
                        map.insert(key, config);
                    }
                }
            }
        }
        Winners { map }
    }

    pub fn get(&self, kind: TargetKind, namespace: &str, name: &str) -> Option<&'a Config> {
        self.map.get(&(kind, namespace, name)).copied()
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
    grants.is_some_and(|grants| {
        grants.iter().any(|g| {
            g.namespace == dest_ns
                && g.from.iter().any(|f| {
                    f.group == MIMIC_GROUP && f.kind == "MimicConfig" && f.namespace == from_ns
                })
                && g.to.iter().any(|t| {
                    t.group.is_empty()
                        && t.kind == "Service"
                        && t.name.as_deref().is_none_or(|n| n.is_empty() || n == name)
                })
        })
    })
}

/// One status condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub ok: bool,
    pub reason: String,
    pub message: String,
}

impl Condition {
    fn ok(reason: &str) -> Condition {
        Condition {
            ok: true,
            reason: reason.into(),
            message: String::new(),
        }
    }

    fn refused(reason: &str, message: String) -> Condition {
        Condition {
            ok: false,
            reason: reason.into(),
            message,
        }
    }
}

/// `status.conditions` of a `MimicConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStatus {
    pub accepted: Condition,
    pub resolved_refs: Condition,
}

/// The status of every `MimicConfig` of the snapshot, keyed by `(namespace, name)`.
pub fn statuses(snapshot: &Snapshot) -> BTreeMap<(String, String), ConfigStatus> {
    let winners = Winners::new(&snapshot.configs);
    let services: BTreeSet<(&str, &str)> = snapshot
        .services
        .iter()
        .map(|s| (s.namespace.as_str(), s.name.as_str()))
        .collect();
    let deployments: BTreeSet<(&str, &str)> = snapshot
        .deployments
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_str()))
        .collect();
    let grants = snapshot.grants.as_deref();
    let mut out = BTreeMap::new();
    for config in &snapshot.configs {
        let ns = config.namespace.as_str();
        let mut missing = BTreeSet::new();
        let mut lost = BTreeSet::new();
        for t in &config.targets {
            let found = match t.kind {
                TargetKind::Deployment => deployments.contains(&(ns, t.name.as_str())),
                TargetKind::Service => services.contains(&(ns, t.name.as_str())),
            };
            let kind = match t.kind {
                TargetKind::Deployment => "Deployment",
                TargetKind::Service => "Service",
            };
            if !found {
                missing.insert(format!("{kind} {}", t.name));
            }
            if let Some(w) = winners.get(t.kind, ns, &t.name)
                && (w.created, &w.name) != (config.created, &config.name)
            {
                lost.insert((
                    format!("{kind} {}", t.name),
                    format!("{}/{}", w.namespace, w.name),
                ));
            }
        }
        let accepted = if let Err(message) = validate(config) {
            Condition::refused("Invalid", message)
        } else if !missing.is_empty() {
            let names: Vec<_> = missing.into_iter().collect();
            Condition::refused("TargetNotFound", format!("not found: {}", names.join(", ")))
        } else if !lost.is_empty() {
            let names: Vec<_> = lost
                .iter()
                .map(|(t, w)| format!("{t} (won by {w})"))
                .collect();
            Condition::refused(
                "Conflicted",
                format!("an older MimicConfig wins: {}", names.join(", ")),
            )
        } else {
            Condition::ok("Accepted")
        };
        let denied: BTreeSet<String> = config
            .destinations
            .iter()
            .flatten()
            .filter(|d| {
                !d.namespace.is_empty() && !d.name.is_empty() && d.namespace != config.namespace
            })
            .filter(|d| !consents(grants, ns, &d.namespace, &d.name))
            .map(|d| format!("{}/{}", d.namespace, d.name))
            .collect();
        let resolved_refs = if denied.is_empty() {
            Condition::ok("ResolvedRefs")
        } else {
            let names: Vec<_> = denied.into_iter().collect();
            Condition::refused(
                "RefNotPermitted",
                format!(
                    "no ReferenceGrant allows a MimicConfig of namespace {ns} to reference the Service: {}",
                    names.join(", ")
                ),
            )
        };
        out.insert(
            (config.namespace.clone(), config.name.clone()),
            ConfigStatus {
                accepted,
                resolved_refs,
            },
        );
    }
    out
}
