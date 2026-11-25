//! The objects of an accepted Gateway (spec §7.10 "Generated objects").

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::accept::Acceptance;
use super::input::Gateway;
use super::{
    ADMIN_PORT, ADMIN_PORT_NAME, GATEWAY_API_VERSION, GATEWAY_LABEL, MAX_NAME, Settings, UID,
};
use crate::inject::literal;

/// `app.kubernetes.io/name` of the generated objects and pods.
const APP_NAME: &str = "mimic-gateway";
/// The pod label that keeps the webhook away (§7.1).
const INJECT_LABEL: &str = "mimic.platform.io/inject";
/// The gateway's entry point.
const COMMAND: &str = "/usr/local/bin/mimic-sidecar";

/// Characters of the Gateway name kept in the hashed form.
const HASHED_PREFIX: usize = 46;
/// Hex digits of the SHA-256 in the hashed form.
const HASH_DIGITS: usize = 10;

/// The name of the Deployment, Service and PodDisruptionBudget of the Gateway `gateway`
/// (GW-01): `<gateway>-mimic` when that is a DNS-1035 label of at most 63 characters
/// (GEP-1762), else a cut of the name, `-`, 10 hex digits of its SHA-256 and `-mimic`.
pub fn name(gateway: &str) -> String {
    let plain = format!("{gateway}-mimic");
    if plain.len() <= MAX_NAME && dns_1035(&plain) {
        return plain;
    }
    let mut cut = String::new();
    if !gateway.starts_with(|c: char| c.is_ascii_lowercase()) {
        cut.push('g');
    }
    cut.extend(gateway.chars().map(|c| if c == '.' { '-' } else { c }));
    let mut out: String = cut.chars().take(HASHED_PREFIX).collect();
    out.truncate(out.trim_end_matches('-').len());
    out.push('-');
    out.push_str(&hex(&Sha256::digest(gateway.as_bytes()), HASH_DIGITS));
    out.push_str("-mimic");
    out
}

/// `[a-z]([-a-z0-9]*[a-z0-9])?`.
fn dns_1035(s: &str) -> bool {
    let b = s.as_bytes();
    let inner = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-';
    match (b.first(), b.last()) {
        (Some(first), Some(last)) => {
            first.is_ascii_lowercase() && *last != b'-' && b.iter().all(inner)
        }
        _ => false,
    }
}

/// The first `digits` lower-case hex digits of `bytes`.
fn hex(bytes: &[u8], digits: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xf)]])
        .take(digits)
        .map(char::from)
        .collect()
}

/// The three objects of an accepted Gateway, as server-side apply bodies.
#[derive(Debug, Clone, PartialEq)]
pub struct Objects {
    pub deployment: Value,
    pub service: Value,
    pub pod_disruption_budget: Value,
}

/// The objects of `gw` (GW-05, GW-06); `None` exactly when it is not accepted (GW-07). The
/// listener order is never read: the ports are the sorted ones of the acceptance.
pub fn objects(gw: &Gateway, acceptance: &Acceptance, settings: &Settings) -> Option<Objects> {
    if !acceptance.is_accepted() {
        return None;
    }
    let ports = acceptance.ports();
    let metadata = json!({
        "labels": {
            "app.kubernetes.io/managed-by": "mimic",
            "app.kubernetes.io/name": APP_NAME,
            "gateway.networking.k8s.io/gateway-class-name": gw.class,
            "gateway.networking.k8s.io/gateway-name": gw.name,
            GATEWAY_LABEL: gw.name,
        },
        "name": name(&gw.name),
        "namespace": gw.namespace,
        // No `blockOwnerDeletion`: the admission policy refuses it (§7.10).
        "ownerReferences": [{
            "apiVersion": GATEWAY_API_VERSION,
            "controller": true,
            "kind": "Gateway",
            "name": gw.name,
            "uid": gw.uid,
        }],
    });
    let selector = json!({GATEWAY_LABEL: gw.name});
    let ports_env = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut container_ports: Vec<Value> = ports
        .iter()
        .map(|p| json!({"containerPort": p, "protocol": "TCP"}))
        .collect();
    container_ports
        .push(json!({"containerPort": ADMIN_PORT, "name": ADMIN_PORT_NAME, "protocol": "TCP"}));
    let service_ports: Vec<Value> = ports
        .iter()
        .map(|p| {
            json!({
                "name": format!("{}-{p}", protocol(acceptance, *p)),
                "port": p,
                "protocol": "TCP",
                "targetPort": p,
            })
        })
        .collect();
    let deployment = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": metadata,
        "spec": {
            "replicas": acceptance.replicas,
            "selector": {"matchLabels": selector},
            "strategy": {"rollingUpdate": {"maxSurge": 1, "maxUnavailable": 0}, "type": "RollingUpdate"},
            "template": {
                "metadata": {"labels": {
                    "app.kubernetes.io/name": APP_NAME,
                    GATEWAY_LABEL: gw.name,
                    INJECT_LABEL: "disabled",
                }},
                "spec": {
                    "automountServiceAccountToken": false,
                    "containers": [{
                        "command": [COMMAND],
                        "env": [
                            {"name": "MIMIC_GATEWAY_PORTS", "value": ports_env},
                            {"name": "MIMIC_LOG", "value": literal(&settings.log)},
                            {"name": "MIMIC_ROLE", "value": "gateway"},
                        ],
                        "image": settings.image,
                        "livenessProbe": {"httpGet": {"path": "/healthz", "port": ADMIN_PORT}},
                        "name": "gateway",
                        "ports": container_ports,
                        "readinessProbe": {"httpGet": {"path": "/readyz", "port": ADMIN_PORT}},
                        "resources": settings.resources,
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "capabilities": {"drop": ["ALL"]},
                            "readOnlyRootFilesystem": true,
                        },
                    }],
                    "enableServiceLinks": false,
                    "securityContext": {
                        "runAsGroup": UID,
                        "runAsNonRoot": true,
                        "runAsUser": UID,
                        "seccompProfile": {"type": "RuntimeDefault"},
                        // Binds ports below 1024 without a capability; allowed by `restricted`.
                        "sysctls": [{"name": "net.ipv4.ip_unprivileged_port_start", "value": "0"}],
                    },
                    "topologySpreadConstraints": [{
                        "labelSelector": {"matchLabels": selector},
                        "maxSkew": 1,
                        "topologyKey": "kubernetes.io/hostname",
                        "whenUnsatisfiable": "ScheduleAnyway",
                    }],
                },
            },
        },
    });
    let service = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata,
        "spec": {
            "ports": service_ports,
            "selector": selector,
            "type": settings.service_type.as_str(),
        },
    });
    let pod_disruption_budget = json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": metadata,
        "spec": {"maxUnavailable": 1, "selector": {"matchLabels": selector}},
    });
    Some(Objects {
        deployment,
        service,
        pod_disruption_budget,
    })
}

/// The protocol of the first accepted listener on `port`, in lower case (`http`).
fn protocol(acceptance: &Acceptance, port: u16) -> String {
    acceptance
        .listeners
        .iter()
        .find(|l| l.accepted.ok && l.port == port)
        .map(|l| l.protocol.to_ascii_lowercase())
        .unwrap_or_default()
}
