//! The status of a Gateway and of a GatewayClass (spec §7.10 "Status"): the conditions, the
//! listener entries, the addresses, and whether the cluster's status needs a write. The time is
//! passed in; the cluster's status is only compared, never copied, but the kept
//! `lastTransitionTime`s.

use serde_json::{Value, json};

use super::accept::{self, Acceptance, ListenerAcceptance};
use super::input::{Gateway, GatewayClass, Observed};
use super::{GATEWAY_API_VERSION, MAX_ADDRESSES};
use crate::mesh::policy::Condition;
use crate::mesh::route::bounded;

/// Longest address value (the CRD's maximum).
const MAX_ADDRESS: usize = 253;

/// What to send for a status.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The cluster holds it already.
    Keep,
    /// The whole server-side apply body.
    Write(Value),
}

/// The Gateway's `Programmed` condition: the first case of §7.10 that applies (GW-08).
pub fn programmed(acceptance: &Acceptance, observed: &Observed) -> Condition {
    if !acceptance.is_accepted() {
        return Condition::refused("Invalid", "not accepted".into());
    }
    if let Some((kind, name)) = &observed.unmanaged {
        return Condition::refused(
            "Invalid",
            bounded(&format!(
                "{} {name} exists and is not managed by mimic",
                kind.as_str()
            )),
        );
    }
    let wanted = acceptance.replicas;
    let available = observed
        .deployment
        .as_ref()
        .map_or(0, |d| d.available_replicas);
    let replicas = format!("{available} of {wanted} replicas available");
    match &observed.deployment {
        Some(d) if d.observed_generation >= d.generation && d.available_replicas > 0 => {
            if d.replica_failure {
                Condition::refused("NoResources", "replica creation failed".into())
            } else if addresses(observed).is_empty() {
                Condition::refused("AddressNotAssigned", "no address assigned".into())
            } else {
                Condition {
                    ok: true,
                    reason: "Programmed".into(),
                    message: replicas,
                }
            }
        }
        _ => Condition::refused("Pending", replicas),
    }
}

/// The Service's addresses: its `clusterIPs` for `ClusterIP` and `NodePort`, the ingress `ip`s
/// then `hostname`s for `LoadBalancer`; empty or over-long values dropped, at most 16.
fn addresses(observed: &Observed) -> Vec<Value> {
    let Some(service) = &observed.service else {
        return vec![];
    };
    let entry = |kind: &str, value: &str| json!({"type": kind, "value": value});
    let fits = |v: &&String| !v.is_empty() && v.len() <= MAX_ADDRESS;
    if service.service_type == "LoadBalancer" {
        let ips = service.ingress.iter().filter_map(|i| i.ip.as_ref());
        let hosts = service.ingress.iter().filter_map(|i| i.hostname.as_ref());
        ips.filter(fits)
            .map(|v| entry("IPAddress", v))
            .chain(hosts.filter(fits).map(|v| entry("Hostname", v)))
            .take(MAX_ADDRESSES)
            .collect()
    } else {
        service
            .cluster_ips
            .iter()
            .filter(fits)
            .map(|v| entry("IPAddress", v))
            .take(MAX_ADDRESSES)
            .collect()
    }
}

/// The Gateway's status body, or `Keep` when the cluster holds it already (GW-09).
pub fn gateway(gw: &Gateway, acceptance: &Acceptance, observed: &Observed, now: &str) -> Plan {
    let programmed = programmed(acceptance, observed);
    let previous = |list: &[Value], kind: &str| list.iter().find(|c| c["type"] == kind).cloned();
    let top = &gw.observed.conditions;
    let mut status = json!({
        "conditions": [
            acceptance.accepted.to_json("Accepted", gw.generation, previous(top, "Accepted").as_ref(), now),
            programmed.to_json("Programmed", gw.generation, previous(top, "Programmed").as_ref(), now),
        ],
        "listeners": acceptance
            .listeners
            .iter()
            .map(|l| listener(gw, l, programmed.ok, now))
            .collect::<Vec<_>>(),
    });
    if programmed.ok {
        status["addresses"] = Value::Array(addresses(observed));
    }
    let held = json!({
        "addresses": gw.observed.addresses,
        "conditions": gw.observed.conditions,
        "listeners": gw.observed.listeners,
    });
    if normalised(&status) == normalised(&held) {
        return Plan::Keep;
    }
    Plan::Write(json!({
        "apiVersion": GATEWAY_API_VERSION,
        "kind": "Gateway",
        "metadata": {"name": gw.name, "namespace": gw.namespace},
        "status": status,
    }))
}

fn listener(gw: &Gateway, l: &ListenerAcceptance, programmed: bool, now: &str) -> Value {
    let before: &[Value] = gw
        .observed
        .listeners
        .iter()
        .find(|e| e["name"] == l.name.as_str())
        .and_then(|e| e["conditions"].as_array())
        .map_or(&[], Vec::as_slice);
    let previous = |kind: &str| before.iter().find(|c| c["type"] == kind);
    let state = if !l.accepted.ok {
        Condition::refused("Invalid", "not accepted".into())
    } else if programmed {
        Condition::ok("Programmed")
    } else {
        Condition::refused("Pending", "gateway not programmed".into())
    };
    let g = gw.generation;
    json!({
        "attachedRoutes": l.attached_routes,
        "conditions": [
            l.accepted.to_json("Accepted", g, previous("Accepted"), now),
            l.conflicted.to_json("Conflicted", g, previous("Conflicted"), now),
            state.to_json("Programmed", g, previous("Programmed"), now),
            l.resolved_refs.to_json("ResolvedRefs", g, previous("ResolvedRefs"), now),
        ],
        "name": l.name,
        "supportedKinds": l
            .supported_kinds
            .iter()
            .map(|k| json!({"group": k.group, "kind": k.kind}))
            .collect::<Vec<_>>(),
    })
}

/// `status` with every `conditions` list sorted by `type` and absent lists as `[]`.
fn normalised(status: &Value) -> Value {
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let listeners: Vec<Value> = list(&status["listeners"])
        .into_iter()
        .map(|mut l| {
            if let Some(conditions) = l.get_mut("conditions") {
                *conditions = Value::Array(by_type(list(conditions)));
            }
            l
        })
        .collect();
    json!({
        "addresses": list(&status["addresses"]),
        "conditions": by_type(list(&status["conditions"])),
        "listeners": listeners,
    })
}

fn by_type(mut conditions: Vec<Value>) -> Vec<Value> {
    conditions.sort_by(|a, b| a["type"].as_str().cmp(&b["type"].as_str()));
    conditions
}

/// The GatewayClass's status body, or `Keep` when the cluster holds it already (GW-09).
pub fn class(class: &GatewayClass, now: &str) -> Plan {
    let previous = class.observed.iter().find(|c| c["type"] == "Accepted");
    let conditions =
        vec![accept::class(class).to_json("Accepted", class.generation, previous, now)];
    if by_type(class.observed.clone()) == conditions {
        return Plan::Keep;
    }
    Plan::Write(json!({
        "apiVersion": GATEWAY_API_VERSION,
        "kind": "GatewayClass",
        "metadata": {"name": class.name},
        "status": {"conditions": conditions},
    }))
}
