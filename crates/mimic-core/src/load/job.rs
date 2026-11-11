//! The objects of a run: the generator Job and its NetworkPolicy (spec §7.8 "Generator").
//! Plain JSON values, so that the controller sends exactly what the tests inspect.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::schedule::pods;

/// The sidecar admin port, on which the probe answers (spec §4).
const ADMIN_PORT: u16 = 15100;
/// Name of the admin port, so that a `PodMonitor` can scrape it (spec §7.8, MON-03).
const ADMIN_PORT_NAME: &str = "mimic-admin";
/// Uid the generator runs as.
const UID: u64 = 65532;
/// Where the credentials Secret is mounted.
const CREDENTIALS_DIR: &str = "/mimic/credentials";
/// The Job annotation naming the credentials Secret (spec §7.8, §11).
const CREDENTIALS_ANNOTATION: &str = "mimic.platform.io/credentials-secret";
/// The projected token: its directory and file name (`MIMIC_LOAD_TOKEN_FILE`).
const TOKEN_DIR: &str = "/var/run/secrets/mimic.platform.io";
const TOKEN_FILE: &str = "load-token";
const TOKEN_AUDIENCE: &str = "mimic.platform.io/load";
const TOKEN_SECONDS: u64 = 600;

/// The name of a run's Job: `mimic-load-<first 12 hex of the uid>`.
pub fn job_name(uid: &str) -> String {
    let hex: String = uid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect();
    format!("mimic-load-{}", hex.to_ascii_lowercase())
}

pub struct JobInput<'a> {
    pub namespace: &'a str,
    /// The `MimicLoadTest`.
    pub name: &'a str,
    pub uid: &'a str,
    pub deployment: &'a str,
    pub concurrency: u64,
    pub duration_ms: u64,
    pub pod_concurrency: u64,
    pub start_timeout_ms: u64,
    pub report_interval_ms: u64,
    pub max_plan: usize,
    pub image: &'a str,
    /// `MIMIC_LOAD_RESOURCES`.
    pub resources: &'a Value,
    pub controller_url: &'a str,
    pub controller_ca: &'a str,
    /// The credentials Secret of the spec.
    pub secret_name: Option<&'a str>,
}

fn labels(name: &str) -> Value {
    json!({
        "app.kubernetes.io/managed-by": "mimic",
        "mimic.platform.io/load-test": name,
    })
}

fn owner(name: &str, uid: &str) -> Value {
    json!([{
        "apiVersion": "mimic.platform.io/v1alpha1",
        "kind": "MimicLoadTest",
        "name": name,
        "uid": uid,
        "controller": true,
        "blockOwnerDeletion": false,
    }])
}

fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// The Job of a run: `ceil(concurrency / pod_concurrency)` Indexed pods, no retry.
pub fn job(input: &JobInput<'_>) -> Value {
    let n = pods(input.concurrency, input.pod_concurrency);
    let mut env = vec![
        (
            "MIMIC_LOAD_RUN",
            format!("{}/{}/{}", input.namespace, input.name, input.uid),
        ),
        ("MIMIC_LOAD_DEPLOYMENT", input.deployment.to_string()),
        ("MIMIC_CONTROLLER_URL", input.controller_url.to_string()),
        ("MIMIC_CONTROLLER_CA", input.controller_ca.to_string()),
        ("MIMIC_LOAD_TOKEN_FILE", format!("{TOKEN_DIR}/{TOKEN_FILE}")),
        (
            "MIMIC_LOAD_REPORT_INTERVAL",
            seconds(input.report_interval_ms),
        ),
        ("MIMIC_LOAD_MAX_PLAN", input.max_plan.to_string()),
    ];
    let mut volumes = vec![json!({
        "name": "load-token",
        "projected": {"sources": [{"serviceAccountToken": {
            "audience": TOKEN_AUDIENCE,
            "expirationSeconds": TOKEN_SECONDS,
            "path": TOKEN_FILE,
        }}]},
    })];
    let mut mounts = vec![json!({"name": "load-token", "mountPath": TOKEN_DIR, "readOnly": true})];
    if let Some(secret) = input.secret_name {
        env.push(("MIMIC_LOAD_CREDENTIALS_DIR", CREDENTIALS_DIR.to_string()));
        volumes.push(json!({
            "name": "credentials",
            "secret": {"secretName": secret, "optional": false, "defaultMode": 0o440},
        }));
        mounts.push(json!({"name": "credentials", "mountPath": CREDENTIALS_DIR, "readOnly": true}));
    }
    let env: Vec<Value> = env
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    let deadline = input.duration_ms.div_ceil(1000) + input.start_timeout_ms.div_ceil(1000) + 60;
    let mut metadata = json!({
        "name": job_name(input.uid),
        "namespace": input.namespace,
        "labels": labels(input.name),
        "ownerReferences": owner(input.name, input.uid),
    });
    // The policy checks the Secret volume against this annotation (spec §11, LDT-17).
    if let Some(secret) = input.secret_name {
        metadata["annotations"] = json!({CREDENTIALS_ANNOTATION: secret});
    }
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": metadata,
        "spec": {
            "completionMode": "Indexed",
            "parallelism": n,
            "completions": n,
            "backoffLimit": 0,
            "activeDeadlineSeconds": deadline,
            "ttlSecondsAfterFinished": 3600,
            "template": {
                "metadata": {"labels": {
                    "app.kubernetes.io/managed-by": "mimic",
                    "mimic.platform.io/inject": "disabled",
                    "mimic.platform.io/load-test": input.name,
                }},
                "spec": {
                    "restartPolicy": "Never",
                    "serviceAccountName": "default",
                    "automountServiceAccountToken": false,
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": UID,
                        // Group-read on the Secret files (mode 0440) needs the group (#414).
                        "fsGroup": UID,
                        "fsGroupChangePolicy": "OnRootMismatch",
                        "seccompProfile": {"type": "RuntimeDefault"},
                    },
                    "containers": [{
                        "name": "mimic-load",
                        "image": input.image,
                        "command": ["/usr/local/bin/mimic-load"],
                        "ports": [{"name": ADMIN_PORT_NAME, "containerPort": ADMIN_PORT, "protocol": "TCP"}],
                        "env": env,
                        "resources": input.resources,
                        "securityContext": {
                            "runAsNonRoot": true,
                            "runAsUser": UID,
                            "allowPrivilegeEscalation": false,
                            "readOnlyRootFilesystem": true,
                            "capabilities": {"drop": ["ALL"]},
                        },
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
        },
    })
}

pub struct PolicyInput<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub uid: &'a str,
    pub deployment: &'a str,
    /// Ports of the plan's endpoints.
    pub ports: &'a [u16],
    pub controller_namespace: &'a str,
    pub controller_labels: &'a BTreeMap<String, String>,
    pub controller_port: u16,
    pub dns_namespace: &'a str,
    pub dns_labels: &'a BTreeMap<String, String>,
    /// `loadTest.metricsFrom`: NetworkPolicy peers allowed on the admin port.
    pub metrics_from: &'a [Value],
}

fn tcp(port: u16) -> Value {
    json!({"protocol": "TCP", "port": port})
}

fn peer(namespace: &str, labels: &BTreeMap<String, String>) -> Value {
    json!({
        "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": namespace}},
        "podSelector": {"matchLabels": labels},
    })
}

/// The NetworkPolicy of a run: the generator reaches the deployment's pods, the controller and
/// DNS, and nothing else.
pub fn network_policy(input: &PolicyInput<'_>) -> Value {
    let mut ports: Vec<u16> = input.ports.to_vec();
    ports.push(ADMIN_PORT);
    ports.sort_unstable();
    ports.dedup();
    let ingress: Vec<Value> = if input.metrics_from.is_empty() {
        vec![]
    } else {
        vec![json!({"from": input.metrics_from, "ports": [tcp(ADMIN_PORT)]})]
    };
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": job_name(input.uid),
            "namespace": input.namespace,
            "labels": labels(input.name),
            "ownerReferences": owner(input.name, input.uid),
        },
        "spec": {
            "podSelector": {"matchLabels": {"mimic.platform.io/load-test": input.name}},
            "policyTypes": ["Ingress", "Egress"],
            "ingress": ingress,
            "egress": [
                {
                    "to": [{"podSelector": {"matchLabels": {"mimic.platform.io/deployment": input.deployment}}}],
                    "ports": ports.into_iter().map(tcp).collect::<Vec<_>>(),
                },
                {
                    "to": [peer(input.controller_namespace, input.controller_labels)],
                    "ports": [tcp(input.controller_port)],
                },
                {
                    "to": [peer(input.dns_namespace, input.dns_labels)],
                    "ports": [
                        {"protocol": "UDP", "port": 53},
                        {"protocol": "TCP", "port": 53},
                    ],
                },
            ],
        },
    })
}
