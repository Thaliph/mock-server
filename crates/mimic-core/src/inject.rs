//! Injection decision (spec §7.1): which pods get the sidecar, which are skipped and why.

use k8s_openapi::api::core::v1::{Container, HTTPGetAction, Pod, Probe, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Label key read on namespaces and pods.
pub const INJECT_LABEL: &str = "mimic.platform.io/inject";
/// Name of the injected sidecar init container, also the idempotence marker.
pub const SIDECAR_NAME: &str = "mimic-sidecar";
/// Name of the injected iptables init container.
pub const INIT_NAME: &str = "mimic-init";
/// Annotation holding the injected version.
pub const INJECTED_ANNOTATION: &str = "mimic.platform.io/injected";
/// Sidecar admin port (probes, `/app-health`).
const ADMIN_PORT: i32 = 15100;
/// Ports the sidecar owns in the pod's network namespace.
pub const RESERVED_PORTS: [i32; 3] = [15001, 15006, 15100];
/// UID of the sidecar, which the app must not use.
pub const PROXY_UID: i64 = 1337;
/// Volume holding the trust bundles, written by the sidecar and the `mimic-trust-<n>` containers.
const TRUST_VOLUME: &str = "mimic-trust";
/// Volume holding the projected token for the sidecar API.
const TOKEN_VOLUME: &str = "mimic-token";
/// Where app containers see their image's trust bundle.
const APP_TRUST_PATH: &str = "/etc/mimic/tls";
/// Prefix of the injected trust init containers.
const TRUST_PREFIX: &str = "mimic-trust-";
/// Options pointing a JVM at the PKCS12 trust store (spec §7.1).
const JAVA_TRUST_OPTIONS: &str = "-Djavax.net.ssl.trustStore=/etc/mimic/tls/truststore.p12 -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=mimic";
/// Namespaces that are never injected, besides the controller's own.
pub const EXCLUDED_NAMESPACES: [&str; 3] = ["kube-system", "kube-public", "kube-node-lease"];

/// Value of the injection label on the pod's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceMode {
    Enabled,
    Selected,
    Absent,
}

/// Why a pod is skipped (annotation `mimic.platform.io/skipped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    HostNetwork,
    UnsupportedOwner,
    PortConflict,
    UidConflict,
    NameConflict,
}

/// Why a pod is left alone without any patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Untouched {
    ExcludedNamespace,
    PodDisabled,
    NotSelected,
    AlreadyInjected,
}

/// Outcome of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Inject the sidecar for this deployment.
    Inject { deployment: String },
    /// Admit with the `skipped` annotation only.
    Skip {
        reason: SkipReason,
        deployment: Option<String>,
    },
    /// Admit unchanged.
    Untouched(Untouched),
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::HostNetwork => "host-network",
            SkipReason::UnsupportedOwner => "unsupported-owner",
            SkipReason::PortConflict => "port-conflict",
            SkipReason::UidConflict => "uid-conflict",
            SkipReason::NameConflict => "name-conflict",
        }
    }
}

impl Untouched {
    pub fn as_str(self) -> &'static str {
        match self {
            Untouched::ExcludedNamespace => "excluded-namespace",
            Untouched::PodDisabled => "pod-disabled",
            Untouched::NotSelected => "not-selected",
            Untouched::AlreadyInjected => "already-injected",
        }
    }
}

/// Deployment owning the pod: its controlling ReplicaSet is named
/// `<deployment>-<pod-template-hash>`.
pub fn deployment_of(pod: &Pod) -> Option<String> {
    let owner = pod
        .metadata
        .owner_references
        .as_ref()?
        .iter()
        .find(|o| o.controller == Some(true))?;
    if owner.kind != "ReplicaSet" || !owner.api_version.starts_with("apps/") {
        return None;
    }
    let hash = pod.metadata.labels.as_ref()?.get("pod-template-hash")?;
    let deployment = owner.name.strip_suffix(hash.as_str())?.strip_suffix('-')?;
    (!hash.is_empty() && !deployment.is_empty()).then(|| deployment.to_string())
}

/// Decides what to do with `pod` created in `namespace`. `workload_exists` tells whether a
/// `MimicWorkload` of that namespace has `spec.deployment` equal to the given name.
pub fn decide(
    pod: &Pod,
    namespace: &str,
    controller_namespace: &str,
    namespace_mode: NamespaceMode,
    workload_exists: impl Fn(&str) -> bool,
) -> Decision {
    if namespace == controller_namespace || EXCLUDED_NAMESPACES.contains(&namespace) {
        return Decision::Untouched(Untouched::ExcludedNamespace);
    }
    let pod_label = pod
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(INJECT_LABEL))
        .map(String::as_str);
    let deployment = deployment_of(pod);
    let selected = match (pod_label, namespace_mode) {
        (Some("disabled"), _) => return Decision::Untouched(Untouched::PodDisabled),
        (Some("enabled"), _) | (_, NamespaceMode::Enabled) => true,
        (_, NamespaceMode::Selected) => deployment.as_deref().is_some_and(&workload_exists),
        (_, NamespaceMode::Absent) => false,
    };
    if !selected {
        return Decision::Untouched(Untouched::NotSelected);
    }
    let spec = pod.spec.as_ref();
    if spec.is_some_and(|s| {
        s.init_containers
            .iter()
            .flatten()
            .any(|c| c.name == SIDECAR_NAME)
    }) {
        return Decision::Untouched(Untouched::AlreadyInjected);
    }
    let skip = |reason| Decision::Skip {
        reason,
        deployment: deployment.clone(),
    };
    let Some(deployment) = deployment.clone() else {
        return skip(SkipReason::UnsupportedOwner);
    };
    let Some(spec) = spec else {
        return Decision::Inject { deployment };
    };
    if spec.host_network == Some(true) {
        return skip(SkipReason::HostNetwork);
    }
    let port_conflict = spec.containers.iter().any(|c| {
        c.ports
            .iter()
            .flatten()
            .any(|p| RESERVED_PORTS.contains(&p.container_port))
    });
    if port_conflict {
        return skip(SkipReason::PortConflict);
    }
    let pod_uid = spec.security_context.as_ref().and_then(|s| s.run_as_user);
    let uid_conflict = pod_uid == Some(PROXY_UID)
        || spec
            .containers
            .iter()
            .any(|c| c.security_context.as_ref().and_then(|s| s.run_as_user) == Some(PROXY_UID));
    if uid_conflict {
        return skip(SkipReason::UidConflict);
    }
    let trust_name = |name: &str| {
        name.strip_prefix(TRUST_PREFIX)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let reserved = |name: &str| name == INIT_NAME || name == SIDECAR_NAME || trust_name(name);
    let mounts_trust_path = |c: &Container| {
        c.volume_mounts
            .iter()
            .flatten()
            .any(|m| m.mount_path.trim_end_matches('/') == APP_TRUST_PATH)
    };
    let name_conflict = spec.containers.iter().any(|c| reserved(&c.name))
        || spec
            .init_containers
            .iter()
            .flatten()
            .any(|c| c.name == INIT_NAME || trust_name(&c.name))
        || spec
            .volumes
            .iter()
            .flatten()
            .any(|v| v.name == TRUST_VOLUME || v.name == TOKEN_VOLUME)
        || spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .any(mounts_trust_path);
    if name_conflict {
        return skip(SkipReason::NameConflict);
    }
    Decision::Inject { deployment }
}

/// A literal `env` value as the kubelet will expand it: `$$` stands for a literal `$`.
fn literal(value: &str) -> String {
    value.replace('$', "$$")
}

/// One RFC 6902 operation of the injection patch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JsonPatchOp {
    pub op: &'static str,
    pub path: String,
    /// Absent (null) for `remove`.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub value: serde_json::Value,
}

/// What the webhook knows about the sidecar it injects (spec §10.2).
#[derive(Debug, Clone, PartialEq)]
pub struct InjectConfig {
    /// Value of the `mimic.platform.io/injected` annotation.
    pub version: String,
    pub sidecar_image: String,
    pub init_image: String,
    pub sidecar_resources: ResourceRequirements,
    /// `MIMIC_LOG` given to the sidecar.
    pub sidecar_log: String,
    /// `MIMIC_CONTROLLER_URL` given to the sidecar (spec §7.1).
    pub controller_url: String,
}

/// What differs per pod in the TLS wiring (spec §7.1, §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsWiring {
    /// The deployment's root certificate, PEM.
    pub root_pem: String,
    /// `spec.tls.exclude` of the deployment, sorted and deduplicated.
    pub exclude: Vec<String>,
    /// `spec.discovery.redact` of the deployment, sorted and deduplicated (spec §6, §7).
    pub redact: Vec<String>,
    /// The webhook CA, PEM, which the sidecar verifies the sidecar API against.
    pub controller_ca: String,
}

/// Injection patch of `pod`, owned by `deployment`.
pub fn patch(
    pod: &Pod,
    deployment: &str,
    cfg: &InjectConfig,
    tls: &TlsWiring,
) -> Result<Vec<JsonPatchOp>, serde_json::Error> {
    let add = |path: String, value: Value| JsonPatchOp {
        op: "add",
        path,
        value,
    };
    let exclusions = excluded_inbound_ports(pod)
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let init = json!({
        "name": INIT_NAME,
        "image": cfg.init_image,
        "env": [{"name": "MIMIC_EXCLUDE_INBOUND_PORTS", "value": literal(&exclusions)}],
        "securityContext": {
            "runAsUser": 0,
            "runAsNonRoot": false,
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"], "add": ["NET_ADMIN", "NET_RAW"]},
            "seccompProfile": {"type": "RuntimeDefault"},
        },
    });
    let field = |name: &str, path: &str| json!({"name": name, "valueFrom": {"fieldRef": {"fieldPath": path}}});
    let rewritten = app_probes(pod);
    let mut sidecar_env = vec![
        json!({"name": "MIMIC_WORKLOAD", "value": literal(deployment)}),
        field("MIMIC_POD_NAMESPACE", "metadata.namespace"),
        field("MIMIC_POD_NAME", "metadata.name"),
        field("MIMIC_POD_IP", "status.podIP"),
        json!({"name": "MIMIC_APP_PROBES", "value": literal(&serde_json::to_string(&rewritten)?)}),
        json!({"name": "MIMIC_LOG", "value": literal(&cfg.sidecar_log)}),
        json!({"name": "MIMIC_TLS_CA", "value": literal(&tls.root_pem)}),
        json!({"name": "MIMIC_CONTROLLER_URL", "value": literal(&cfg.controller_url)}),
        json!({"name": "MIMIC_CONTROLLER_CA", "value": literal(&tls.controller_ca)}),
    ];
    if !tls.exclude.is_empty() {
        sidecar_env
            .push(json!({"name": "MIMIC_TLS_EXCLUDE", "value": literal(&tls.exclude.join(","))}));
    }
    if !tls.redact.is_empty() {
        sidecar_env.push(json!({"name": "MIMIC_DISCOVERY_REDACT_NAMES", "value": literal(&tls.redact.join(","))}));
    }
    let sidecar = json!({
        "name": SIDECAR_NAME,
        "image": cfg.sidecar_image,
        "command": ["/usr/local/bin/mimic-sidecar"],
        "restartPolicy": "Always",
        "env": sidecar_env,
        "volumeMounts": [
            {"name": TRUST_VOLUME, "mountPath": "/mimic"},
            {"name": TOKEN_VOLUME, "mountPath": "/var/run/secrets/mimic.platform.io", "readOnly": true},
        ],
        "resources": serde_json::to_value(&cfg.sidecar_resources)?,
        "startupProbe": {
            "httpGet": {"path": "/readyz", "port": ADMIN_PORT},
            "periodSeconds": 1,
            "failureThreshold": 60,
        },
        "livenessProbe": {
            "httpGet": {"path": "/healthz", "port": ADMIN_PORT},
            "periodSeconds": 10,
        },
        "securityContext": {
            "runAsUser": PROXY_UID,
            "runAsGroup": PROXY_UID,
            "runAsNonRoot": true,
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
            "seccompProfile": {"type": "RuntimeDefault"},
        },
    });

    let spec = pod.spec.as_ref();
    let mut ops = Vec::new();
    let app_inits: &[Container] = spec
        .and_then(|s| s.init_containers.as_deref())
        .unwrap_or_default();
    let apps: &[Container] = spec.map_or(&[], |s| s.containers.as_slice());
    let images = distinct_images(app_inits.iter().chain(apps));
    let mut injected = vec![init, sidecar];
    for (n, (image, pull_policy)) in images.iter().enumerate() {
        injected.push(trust_container(n, image, *pull_policy, cfg, tls));
    }
    let shift = injected.len();
    if app_inits.is_empty() {
        ops.push(add("/spec/initContainers".into(), Value::Array(injected)));
    } else {
        for (i, container) in injected.into_iter().enumerate() {
            ops.push(add(format!("/spec/initContainers/{i}"), container));
        }
    }
    let volumes = json!([
        {"name": TRUST_VOLUME, "emptyDir": {"sizeLimit": "16Mi"}},
        {"name": TOKEN_VOLUME, "projected": {"sources": [{"serviceAccountToken": {
            "audience": "mimic.platform.io", "expirationSeconds": 3600, "path": "token",
        }}]}},
    ]);
    if spec.is_some_and(|s| s.volumes.as_ref().is_some_and(|v| !v.is_empty())) {
        for volume in volumes.as_array().into_iter().flatten() {
            ops.push(add("/spec/volumes/-".into(), volume.clone()));
        }
    } else {
        ops.push(add("/spec/volumes".into(), volumes));
    }
    let bundle_of = |c: &Container| images.iter().position(|(i, _)| Some(i) == c.image.as_ref());
    for (i, c) in app_inits.iter().enumerate() {
        let base = format!("/spec/initContainers/{}", shift + i);
        wire_app_container(&mut ops, &base, c, bundle_of(c));
    }
    for (i, c) in apps.iter().enumerate() {
        wire_app_container(&mut ops, &format!("/spec/containers/{i}"), c, bundle_of(c));
    }
    if pod.metadata.annotations.is_some() {
        let key = INJECTED_ANNOTATION.replace('/', "~1");
        ops.push(add(
            format!("/metadata/annotations/{key}"),
            json!(cfg.version),
        ));
    } else {
        ops.push(add(
            "/metadata/annotations".into(),
            json!({ INJECTED_ANNOTATION: cfg.version }),
        ));
    }
    for (i, c) in spec.iter().flat_map(|s| &s.containers).enumerate() {
        for (field, kind, probe) in probes_of(c) {
            let Some(probe) =
                probe.filter(|_| rewritten.contains_key(&format!("{}/{kind}", c.name)))
            else {
                continue;
            };
            let base = format!("/spec/containers/{i}/{field}");
            let value = json!({
                "path": format!("/app-health/{}/{kind}", c.name),
                "port": ADMIN_PORT,
                "scheme": "HTTP",
            });
            if probe.http_get.is_some() {
                ops.push(JsonPatchOp {
                    op: "replace",
                    path: format!("{base}/httpGet"),
                    value,
                });
                continue;
            }
            let old = if probe.tcp_socket.is_some() {
                "tcpSocket"
            } else {
                "grpc"
            };
            ops.push(JsonPatchOp {
                op: "remove",
                path: format!("{base}/{old}"),
                value: Value::Null,
            });
            ops.push(add(format!("{base}/httpGet"), value));
        }
    }
    Ok(ops)
}

/// Images of `containers` with the pull policy of the first container running each, in order
/// of first appearance. A container without an image is not an app image (the API server
/// rejects it) and gets no bundle.
fn distinct_images<'a>(
    containers: impl Iterator<Item = &'a Container>,
) -> Vec<(String, Option<&'a str>)> {
    let mut images: Vec<(String, Option<&str>)> = Vec::new();
    for c in containers {
        if let Some(image) = &c.image
            && !images.iter().any(|(i, _)| i == image)
        {
            images.push((image.clone(), c.image_pull_policy.as_deref()));
        }
    }
    images
}

/// The `mimic-trust-<n>` init container, which runs mimic's binary inside the app image.
fn trust_container(
    n: usize,
    image: &str,
    pull_policy: Option<&str>,
    cfg: &InjectConfig,
    tls: &TlsWiring,
) -> Value {
    let mut container = json!({
        "name": format!("{TRUST_PREFIX}{n}"),
        "image": image,
        "command": ["/mimic/bin/mimic-trust"],
        "env": [
            {"name": "MIMIC_TRUST_CA", "value": literal(&tls.root_pem)},
            {"name": "MIMIC_TRUST_OUT", "value": format!("/mimic/trust/{n}")},
            {"name": "MIMIC_LOG", "value": literal(&cfg.sidecar_log)},
        ],
        "volumeMounts": [{"name": TRUST_VOLUME, "mountPath": "/mimic"}],
        "resources": {
            "requests": {"cpu": "10m", "memory": "16Mi"},
            "limits": {"memory": "64Mi"},
        },
        "securityContext": {
            "runAsUser": PROXY_UID,
            "runAsGroup": PROXY_UID,
            "runAsNonRoot": true,
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
            "seccompProfile": {"type": "RuntimeDefault"},
        },
    });
    if let Some(policy) = pull_policy {
        container["imagePullPolicy"] = json!(policy);
    }
    container
}

/// Mounts the bundle of `c`'s image and sets the trust variables it does not define itself.
/// `base` is the JSON pointer of the container in the patched pod.
fn wire_app_container(
    ops: &mut Vec<JsonPatchOp>,
    base: &str,
    c: &Container,
    bundle: Option<usize>,
) {
    let Some(n) = bundle else { return };
    let add = |path: String, value: Value| JsonPatchOp {
        op: "add",
        path,
        value,
    };
    let mount = json!({
        "name": TRUST_VOLUME,
        "mountPath": APP_TRUST_PATH,
        "readOnly": true,
        "subPath": format!("trust/{n}"),
    });
    if c.volume_mounts.as_ref().is_some_and(|m| !m.is_empty()) {
        ops.push(add(format!("{base}/volumeMounts/-"), mount));
    } else {
        ops.push(add(format!("{base}/volumeMounts"), json!([mount])));
    }
    let bundle_path = format!("{APP_TRUST_PATH}/ca-bundle.pem");
    let wanted = [
        ("SSL_CERT_FILE", bundle_path.as_str()),
        ("REQUESTS_CA_BUNDLE", bundle_path.as_str()),
        ("CURL_CA_BUNDLE", bundle_path.as_str()),
        ("NODE_EXTRA_CA_CERTS", "/etc/mimic/tls/node-extra-ca.pem"),
        ("JAVA_TOOL_OPTIONS", JAVA_TRUST_OPTIONS),
    ];
    let env = c.env.as_deref().unwrap_or_default();
    let mut added = Vec::new();
    for (name, value) in wanted {
        match env.iter().position(|e| e.name == name) {
            None => added.push(json!({"name": name, "value": value})),
            // The app's own options come later and win; `$(VAR)` references stay the app's.
            Some(i) if name == "JAVA_TOOL_OPTIONS" && env[i].value_from.is_none() => {
                let own = env[i].value.as_deref().unwrap_or_default();
                let value = if own.is_empty() {
                    value.to_string()
                } else {
                    format!("{value} {own}")
                };
                ops.push(JsonPatchOp {
                    op: "add",
                    path: format!("{base}/env/{i}/value"),
                    value: json!(value),
                });
            }
            Some(_) => {}
        }
    }
    if env.is_empty() {
        ops.push(add(format!("{base}/env"), Value::Array(added)));
    } else {
        for var in added {
            ops.push(add(format!("{base}/env/-"), var));
        }
    }
}

/// Liveness, readiness and startup probes of `c` with the field name and the sidecar's kind.
fn probes_of(c: &Container) -> [(&'static str, &'static str, Option<&Probe>); 3] {
    [
        ("livenessProbe", "livez", c.liveness_probe.as_ref()),
        ("readinessProbe", "readyz", c.readiness_probe.as_ref()),
        ("startupProbe", "startupz", c.startup_probe.as_ref()),
    ]
}

/// Port number behind `port`, a name being looked up in the container's own ports.
fn resolve(c: &Container, port: &IntOrString) -> Option<i32> {
    match port {
        IntOrString::Int(n) => Some(*n),
        IntOrString::String(name) => c
            .ports
            .iter()
            .flatten()
            .find(|p| p.name.as_deref() == Some(name))
            .map(|p| p.container_port),
    }
}

/// Inbound ports `mimic-init` must not redirect (spec §4): only the sidecar admin port, probe
/// ports stay intercepted because their probes go through the sidecar.
pub fn excluded_inbound_ports(_pod: &Pod) -> Vec<u16> {
    vec![ADMIN_PORT as u16]
}

/// One original probe as handed to the sidecar in `MIMIC_APP_PROBES`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum AppProbe {
    /// `tcpSocket`: the sidecar connects to `host` (default the pod IP) and `port`.
    #[serde(rename_all = "camelCase")]
    Tcp {
        tcp_socket: TcpTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<i32>,
    },
    /// `grpc`: the sidecar calls `grpc.health.v1.Health/Check` on the pod IP and `port`.
    #[serde(rename_all = "camelCase")]
    Grpc {
        grpc: GrpcTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<i32>,
    },
    /// `httpGet`: the fields of the `HTTPGetAction`.
    #[serde(rename_all = "camelCase")]
    Http {
        #[serde(flatten)]
        get: HTTPGetAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TcpTarget {
    pub port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GrpcTarget {
    pub port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// Original `httpGet`, `tcpSocket` and `grpc` probes of the app containers, keyed
/// `<container>/<livez|readyz|startupz>`, named ports resolved; a probe whose named port does
/// not resolve is left out (and not rewritten), so it fails as it would without mimic.
pub fn app_probes(pod: &Pod) -> BTreeMap<String, AppProbe> {
    let mut out = BTreeMap::new();
    for c in pod.spec.iter().flat_map(|s| &s.containers) {
        for (_, kind, probe) in probes_of(c) {
            let Some(probe) = probe else { continue };
            let timeout_seconds = probe.timeout_seconds;
            let app = if let Some(get) = &probe.http_get {
                resolve(c, &get.port).map(|port| AppProbe::Http {
                    get: HTTPGetAction {
                        port: IntOrString::Int(port),
                        ..get.clone()
                    },
                    timeout_seconds,
                })
            } else if let Some(tcp) = &probe.tcp_socket {
                resolve(c, &tcp.port).map(|port| AppProbe::Tcp {
                    tcp_socket: TcpTarget {
                        port,
                        host: tcp.host.clone().filter(|h| !h.is_empty()),
                    },
                    timeout_seconds,
                })
            } else {
                probe.grpc.as_ref().map(|g| AppProbe::Grpc {
                    grpc: GrpcTarget {
                        port: g.port,
                        service: g.service.clone().filter(|s| !s.is_empty()),
                    },
                    timeout_seconds,
                })
            };
            if let Some(app) = app {
                out.insert(format!("{}/{kind}", c.name), app);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, ContainerPort, PodSecurityContext, PodSpec, SecurityContext,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
    use std::collections::BTreeMap;

    fn pod() -> Pod {
        Pod {
            metadata: ObjectMeta {
                labels: Some(BTreeMap::from([(
                    "pod-template-hash".to_string(),
                    "5d8f7c9b6".to_string(),
                )])),
                owner_references: Some(vec![OwnerReference {
                    api_version: "apps/v1".into(),
                    kind: "ReplicaSet".into(),
                    name: "shop-api-5d8f7c9b6".into(),
                    controller: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "app".into(),
                    image: Some("app:1".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn with_label(mut p: Pod, v: &str) -> Pod {
        p.metadata
            .labels
            .get_or_insert_default()
            .insert(INJECT_LABEL.into(), v.into());
        p
    }

    fn spec(p: &mut Pod) -> &mut PodSpec {
        p.spec.as_mut().unwrap()
    }

    fn owner(p: &mut Pod) -> &mut OwnerReference {
        &mut p.metadata.owner_references.as_mut().unwrap()[0]
    }

    fn run(p: &Pod, ns: NamespaceMode, workload: bool) -> Decision {
        decide(p, "shop", "mimic-system", ns, |d| {
            workload && d == "shop-api"
        })
    }

    fn injected() -> Decision {
        Decision::Inject {
            deployment: "shop-api".into(),
        }
    }

    fn not_selected() -> Decision {
        Decision::Untouched(Untouched::NotSelected)
    }

    fn skip(reason: SkipReason) -> Decision {
        Decision::Skip {
            reason,
            deployment: Some("shop-api".into()),
        }
    }

    #[test]
    fn inj_01_selection_table() {
        use NamespaceMode::*;
        for workload in [false, true] {
            for ns in [Enabled, Selected, Absent] {
                assert_eq!(
                    run(&with_label(pod(), "disabled"), ns, workload),
                    Decision::Untouched(Untouched::PodDisabled)
                );
                assert_eq!(run(&with_label(pod(), "enabled"), ns, workload), injected());
            }
            assert_eq!(run(&pod(), Enabled, workload), injected());
            assert_eq!(run(&pod(), Absent, workload), not_selected());
            assert_eq!(
                run(&pod(), Selected, workload),
                if workload { injected() } else { not_selected() }
            );
        }
    }

    #[test]
    fn inj_01_selected_matches_only_its_own_deployment() {
        let d = decide(
            &pod(),
            "shop",
            "mimic-system",
            NamespaceMode::Selected,
            |d| d == "other",
        );
        assert_eq!(d, not_selected());
    }

    #[test]
    fn inj_01_excluded_namespaces_are_never_injected() {
        for ns in [
            "kube-system",
            "kube-public",
            "kube-node-lease",
            "mimic-system",
        ] {
            let p = with_label(pod(), "enabled");
            let d = decide(&p, ns, "mimic-system", NamespaceMode::Enabled, |_| true);
            assert_eq!(d, Decision::Untouched(Untouched::ExcludedNamespace), "{ns}");
        }
    }

    #[test]
    fn inj_01_deployment_from_replicaset_and_hash() {
        assert_eq!(deployment_of(&pod()).as_deref(), Some("shop-api"));
        let mut p = pod();
        owner(&mut p).name = "a-b-2-5d8f7c9b6".into();
        assert_eq!(deployment_of(&p).as_deref(), Some("a-b-2"));
    }

    #[test]
    fn inj_01_unsupported_owner() {
        let cases: Vec<fn(&mut Pod)> = vec![
            |p| p.metadata.owner_references = None,
            |p| owner(p).kind = "StatefulSet".into(),
            |p| owner(p).controller = None,
            |p| p.metadata.labels = None,
            |p| owner(p).name = "shop-api-other".into(),
            |p| owner(p).name = "5d8f7c9b6".into(),
            |p| owner(p).name = "-5d8f7c9b6".into(),
        ];
        for (i, mutate) in cases.iter().enumerate() {
            let mut p = pod();
            mutate(&mut p);
            assert_eq!(deployment_of(&p), None, "case {i}");
            // A pod labelled `enabled` is skipped with the reason; an unlabelled pod of a
            // selected namespace is simply not selected.
            let mut labelled = p.clone();
            labelled
                .metadata
                .labels
                .get_or_insert_default()
                .insert(INJECT_LABEL.into(), "enabled".into());
            assert_eq!(
                run(&labelled, NamespaceMode::Selected, true),
                Decision::Skip {
                    reason: SkipReason::UnsupportedOwner,
                    deployment: None
                },
                "case {i}"
            );
            assert_eq!(
                run(&p, NamespaceMode::Selected, true),
                not_selected(),
                "case {i}"
            );
        }
    }

    #[test]
    fn inj_02_host_network() {
        let mut p = pod();
        spec(&mut p).host_network = Some(true);
        assert_eq!(
            run(&p, NamespaceMode::Enabled, false),
            skip(SkipReason::HostNetwork)
        );
        assert_eq!(SkipReason::HostNetwork.as_str(), "host-network");
    }

    #[test]
    fn inj_02_port_conflict() {
        for port in [15001, 15006, 15100] {
            let mut p = pod();
            spec(&mut p).containers[0].ports = Some(vec![ContainerPort {
                container_port: port,
                ..Default::default()
            }]);
            assert_eq!(
                run(&p, NamespaceMode::Enabled, false),
                skip(SkipReason::PortConflict)
            );
        }
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![ContainerPort {
            container_port: 8080,
            ..Default::default()
        }]);
        assert_eq!(run(&p, NamespaceMode::Enabled, false), injected());
    }

    #[test]
    fn inj_02_uid_conflict() {
        let mut p = pod();
        spec(&mut p).security_context = Some(PodSecurityContext {
            run_as_user: Some(1337),
            ..Default::default()
        });
        assert_eq!(
            run(&p, NamespaceMode::Enabled, false),
            skip(SkipReason::UidConflict)
        );
        let mut p = pod();
        spec(&mut p).containers[0].security_context = Some(SecurityContext {
            run_as_user: Some(1337),
            ..Default::default()
        });
        assert_eq!(
            run(&p, NamespaceMode::Enabled, false),
            skip(SkipReason::UidConflict)
        );
        let mut p = pod();
        spec(&mut p).containers[0].security_context = Some(SecurityContext {
            run_as_user: Some(65532),
            ..Default::default()
        });
        assert_eq!(run(&p, NamespaceMode::Enabled, false), injected());
    }

    #[test]
    fn inj_02_already_injected_is_untouched() {
        let mut p = pod();
        spec(&mut p).init_containers = Some(vec![Container {
            name: SIDECAR_NAME.into(),
            ..Default::default()
        }]);
        let untouched = Decision::Untouched(Untouched::AlreadyInjected);
        assert_eq!(run(&p, NamespaceMode::Enabled, false), untouched);
        // Even when it would otherwise be skipped.
        spec(&mut p).host_network = Some(true);
        assert_eq!(run(&p, NamespaceMode::Enabled, false), untouched);
    }

    // ---- patch (INJ-03, INJ-04) ----

    use k8s_openapi::api::core::v1::{GRPCAction, Probe, TCPSocketAction};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
    use serde_json::{Value, json};

    fn cfg() -> InjectConfig {
        InjectConfig {
            version: "0.1.0".into(),
            sidecar_image: "ghcr.io/x/mimic-sidecar@sha256:aa".into(),
            init_image: "ghcr.io/x/mimic-init@sha256:bb".into(),
            sidecar_resources: serde_json::from_value(json!({
                "requests": {"cpu": "50m", "memory": "64Mi"},
                "limits": {"memory": "128Mi"}
            }))
            .unwrap(),
            sidecar_log: "debug".into(),
            controller_url: "https://mimic.mimic-system.svc:8444".into(),
        }
    }

    fn wiring() -> TlsWiring {
        TlsWiring {
            root_pem: "-----BEGIN CERTIFICATE-----\nROOT $x\n-----END CERTIFICATE-----\n".into(),
            exclude: Vec::new(),
            redact: Vec::new(),
            controller_ca: "-----BEGIN CERTIFICATE-----\nWEBHOOK\n-----END CERTIFICATE-----\n"
                .into(),
        }
    }

    fn apply(p: &Pod) -> Value {
        apply_with(p, &wiring())
    }

    fn apply_with(p: &Pod, tls: &TlsWiring) -> Value {
        let ops = patch(p, "shop-api", &cfg(), tls).unwrap();
        let mut doc = serde_json::to_value(p).unwrap();
        let ops: json_patch::Patch =
            serde_json::from_value(serde_json::to_value(&ops).unwrap()).unwrap();
        json_patch::patch(&mut doc, &ops).unwrap();
        doc
    }

    fn http(port: IntOrString, path: &str) -> Probe {
        Probe {
            http_get: Some(HTTPGetAction {
                port,
                path: Some(path.into()),
                scheme: Some("HTTPS".into()),
                host: Some("localhost".into()),
                ..Default::default()
            }),
            period_seconds: Some(7),
            timeout_seconds: Some(3),
            failure_threshold: Some(5),
            ..Default::default()
        }
    }

    fn tcp(port: IntOrString) -> Probe {
        Probe {
            tcp_socket: Some(TCPSocketAction { port, host: None }),
            ..Default::default()
        }
    }

    fn grpc(port: i32) -> Probe {
        Probe {
            grpc: Some(GRPCAction {
                port,
                service: None,
            }),
            ..Default::default()
        }
    }

    fn named(name: &str, port: i32) -> ContainerPort {
        ContainerPort {
            name: Some(name.into()),
            container_port: port,
            ..Default::default()
        }
    }

    fn init_names(doc: &Value) -> Vec<&str> {
        doc["spec"]["initContainers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn inj_21_dollars_of_literal_env_values_are_doubled() {
        let mut p = pod();
        let mut probe = http(IntOrString::Int(8080), "/x/$(HOME)/$$");
        probe.http_get.as_mut().unwrap().http_headers =
            Some(vec![k8s_openapi::api::core::v1::HTTPHeader {
                name: "X".into(),
                value: "a$b".into(),
            }]);
        spec(&mut p).containers[0].liveness_probe = Some(probe);
        let doc = apply(&p);
        let mut seen = 0;
        for c in doc["spec"]["initContainers"].as_array().unwrap() {
            for e in c["env"].as_array().unwrap() {
                let Some(v) = e["value"].as_str() else {
                    continue;
                };
                // Kubelet expansion: `$$` is a literal `$`; a lone `$` or `$(VAR)` would differ.
                assert!(!v.replace("$$", "").contains('$'), "{}: {v}", e["name"]);
                if e["name"] == "MIMIC_APP_PROBES" {
                    let decoded: BTreeMap<String, AppProbe> =
                        serde_json::from_str(&v.replace("$$", "$")).unwrap();
                    assert_eq!(decoded, app_probes(&p));
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 1);
        assert!(
            doc["spec"]["initContainers"][1]["env"][4]["value"]
                .as_str()
                .unwrap()
                .contains("$$(HOME)/$$$$")
        );
    }

    #[test]
    fn inj_21_name_conflict() {
        let named = |n: &str| Container {
            name: n.into(),
            ..Default::default()
        };
        for n in ["mimic-init", "mimic-sidecar"] {
            let mut p = pod();
            spec(&mut p).containers.push(named(n));
            assert_eq!(
                run(&p, NamespaceMode::Enabled, false),
                skip(SkipReason::NameConflict),
                "{n}"
            );
        }
        let mut p = pod();
        spec(&mut p).init_containers = Some(vec![named("mimic-init")]);
        assert_eq!(
            run(&p, NamespaceMode::Enabled, false),
            skip(SkipReason::NameConflict)
        );
        assert_eq!(SkipReason::NameConflict.as_str(), "name-conflict");
        // With the sidecar present the pod is already injected.
        spec(&mut p)
            .init_containers
            .as_mut()
            .unwrap()
            .push(named(SIDECAR_NAME));
        assert_eq!(
            run(&p, NamespaceMode::Enabled, false),
            Decision::Untouched(Untouched::AlreadyInjected)
        );
    }

    #[test]
    fn inj_03_init_containers_go_first_also_when_none() {
        // The one app image has its trust container after the sidecar (TLS-11).
        assert_eq!(
            init_names(&apply(&pod())),
            ["mimic-init", "mimic-sidecar", "mimic-trust-0"]
        );
        let mut p = pod();
        spec(&mut p).init_containers = Some(vec![Container {
            name: "migrate".into(),
            ..Default::default()
        }]);
        assert_eq!(
            init_names(&apply(&p)),
            ["mimic-init", "mimic-sidecar", "mimic-trust-0", "migrate"]
        );
    }

    #[test]
    fn inj_03_annotation_added_with_and_without_existing_ones() {
        let doc = apply(&pod());
        assert_eq!(
            doc["metadata"]["annotations"],
            json!({"mimic.platform.io/injected": "0.1.0"})
        );
        let mut p = pod();
        p.metadata.annotations = Some(BTreeMap::from([("a".to_string(), "b".to_string())]));
        let doc = apply(&p);
        assert_eq!(
            doc["metadata"]["annotations"],
            json!({"a": "b", "mimic.platform.io/injected": "0.1.0"})
        );
    }

    #[test]
    fn inj_03_patch_is_deterministic() {
        let mut p = pod();
        spec(&mut p).containers[0].readiness_probe = Some(http(IntOrString::Int(8080), "/r"));
        spec(&mut p).containers[0].liveness_probe = Some(http(IntOrString::Int(8080), "/l"));
        let a = serde_json::to_string(&patch(&p, "shop-api", &cfg(), &wiring()).unwrap()).unwrap();
        let b = serde_json::to_string(&patch(&p, "shop-api", &cfg(), &wiring()).unwrap()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn inj_03_init_container_security() {
        let doc = apply(&pod());
        let init = &doc["spec"]["initContainers"][0];
        assert_eq!(init["image"], "ghcr.io/x/mimic-init@sha256:bb");
        assert_eq!(
            init["securityContext"],
            json!({
                "runAsUser": 0,
                "runAsNonRoot": false,
                "allowPrivilegeEscalation": false,
                "readOnlyRootFilesystem": true,
                "capabilities": {"drop": ["ALL"], "add": ["NET_ADMIN", "NET_RAW"]},
                "seccompProfile": {"type": "RuntimeDefault"}
            })
        );
        assert!(init.get("volumeMounts").is_none());
        assert!(init.get("restartPolicy").is_none());
    }

    #[test]
    fn inj_03_sidecar_container() {
        let doc = apply(&pod());
        let sc = &doc["spec"]["initContainers"][1];
        assert_eq!(sc["image"], "ghcr.io/x/mimic-sidecar@sha256:aa");
        // The shared image's entrypoint is the controller.
        assert_eq!(sc["command"], json!(["/usr/local/bin/mimic-sidecar"]));
        assert_eq!(sc["restartPolicy"], "Always");
        assert_eq!(
            sc["securityContext"],
            json!({
                "runAsUser": 1337,
                "runAsGroup": 1337,
                "runAsNonRoot": true,
                "allowPrivilegeEscalation": false,
                "readOnlyRootFilesystem": true,
                "capabilities": {"drop": ["ALL"]},
                "seccompProfile": {"type": "RuntimeDefault"}
            })
        );
        assert_eq!(
            sc["startupProbe"]["httpGet"],
            json!({"path": "/readyz", "port": 15100})
        );
        assert_eq!(
            sc["livenessProbe"]["httpGet"],
            json!({"path": "/healthz", "port": 15100})
        );
        assert_eq!(
            sc["resources"],
            json!({"requests": {"cpu": "50m", "memory": "64Mi"}, "limits": {"memory": "128Mi"}})
        );
        // Mounts and TLS variables are TLS-11's.
        let env: BTreeMap<_, _> = sc["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap(), e))
            .collect();
        assert_eq!(env["MIMIC_WORKLOAD"]["value"], "shop-api");
        assert_eq!(env["MIMIC_LOG"]["value"], "debug");
        for (name, field) in [
            ("MIMIC_POD_NAMESPACE", "metadata.namespace"),
            ("MIMIC_POD_NAME", "metadata.name"),
            ("MIMIC_POD_IP", "status.podIP"),
        ] {
            assert_eq!(env[name]["valueFrom"]["fieldRef"]["fieldPath"], field);
        }
        assert_eq!(env["MIMIC_APP_PROBES"]["value"], "{}");
    }

    #[test]
    fn inj_03_app_containers_change_only_http_probes() {
        let mut p = pod();
        {
            let c = &mut spec(&mut p).containers[0];
            c.env = Some(vec![]);
            c.readiness_probe = Some(http(IntOrString::Int(8080), "/ready"));
            c.liveness_probe = Some(Probe {
                exec: Some(Default::default()),
                ..Default::default()
            });
        }
        let mut expected = serde_json::to_value(&p).unwrap();
        expected["spec"]["containers"][0]["env"] = json!([]);
        expected["spec"]["containers"][0]["readinessProbe"]["httpGet"] =
            json!({"path": "/app-health/app/readyz", "port": 15100, "scheme": "HTTP"});
        // Everything else is the original, init containers and annotation aside.
        let mut after = apply(&p);
        // The trust wiring (TLS-11) is the only other change: its mount and variables.
        let c = after["spec"]["containers"][0].as_object_mut().unwrap();
        assert_eq!(
            c.remove("volumeMounts").unwrap().as_array().unwrap().len(),
            1
        );
        c.insert("env".into(), json!([]));
        let spec_map = after["spec"].as_object_mut().unwrap();
        spec_map.remove("volumes");
        spec_map.remove("initContainers");
        after["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("annotations");
        assert_eq!(after, expected);
        let rp = &after["spec"]["containers"][0]["readinessProbe"];
        assert_eq!(rp["periodSeconds"], 7);
        assert_eq!(rp["timeoutSeconds"], 3);
        assert_eq!(rp["failureThreshold"], 5);
    }

    #[test]
    fn inj_04_probe_ports_are_not_excluded() {
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![named("db", 5432), named("web", 8080)]);
        spec(&mut p).containers[0].liveness_probe = Some(tcp(IntOrString::String("db".into())));
        spec(&mut p).containers[0].readiness_probe = Some(tcp(IntOrString::Int(5432)));
        spec(&mut p).containers[0].startup_probe = Some(grpc(50051));
        assert_eq!(excluded_inbound_ports(&p), vec![15100]);
        let doc = apply(&p);
        assert_eq!(
            doc["spec"]["initContainers"][0]["env"],
            json!([{"name": "MIMIC_EXCLUDE_INBOUND_PORTS", "value": "15100"}])
        );
    }

    #[test]
    fn inj_04_tcp_and_grpc_probes_are_rewritten() {
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![named("db", 5432)]);
        let mut live = tcp(IntOrString::String("db".into()));
        live.period_seconds = Some(7);
        live.failure_threshold = Some(5);
        live.timeout_seconds = Some(2);
        spec(&mut p).containers[0].liveness_probe = Some(live);
        let mut ready = grpc(50051);
        ready.grpc.as_mut().unwrap().service = Some("svc.Name".into());
        spec(&mut p).containers[0].readiness_probe = Some(ready);
        spec(&mut p).containers[0].startup_probe = Some(tcp(IntOrString::String("nope".into())));
        let probes = app_probes(&p);
        assert_eq!(
            probes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["app/livez", "app/readyz"]
        );
        assert_eq!(
            serde_json::to_value(&probes["app/livez"]).unwrap(),
            json!({"tcpSocket": {"port": 5432}, "timeoutSeconds": 2})
        );
        assert_eq!(
            serde_json::to_value(&probes["app/readyz"]).unwrap(),
            json!({"grpc": {"port": 50051, "service": "svc.Name"}})
        );
        let doc = apply(&p);
        let c = &doc["spec"]["containers"][0];
        assert_eq!(
            c["livenessProbe"],
            json!({
                "httpGet": {"path": "/app-health/app/livez", "port": 15100, "scheme": "HTTP"},
                "periodSeconds": 7, "failureThreshold": 5, "timeoutSeconds": 2,
            })
        );
        assert_eq!(
            c["readinessProbe"],
            json!({"httpGet": {"path": "/app-health/app/readyz", "port": 15100, "scheme": "HTTP"}})
        );
        // An unresolvable name is left to fail as it would without mimic.
        assert_eq!(c["startupProbe"], json!({"tcpSocket": {"port": "nope"}}));
        let value = doc["spec"]["initContainers"][1]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "MIMIC_APP_PROBES")
            .unwrap()["value"]
            .as_str()
            .unwrap();
        let parsed: BTreeMap<String, AppProbe> = serde_json::from_str(value).unwrap();
        assert_eq!(parsed, probes);
    }

    #[test]
    fn inj_04_http_probe_ports_are_not_excluded() {
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![named("web", 8080)]);
        spec(&mut p).containers[0].liveness_probe =
            Some(http(IntOrString::String("web".into()), "/l"));
        spec(&mut p).containers[0].readiness_probe = Some(http(IntOrString::Int(9090), "/r"));
        assert_eq!(excluded_inbound_ports(&p), vec![15100]);
    }

    #[test]
    fn inj_04_app_probes_and_rewrite() {
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![named("web", 8080)]);
        spec(&mut p).containers[0].liveness_probe =
            Some(http(IntOrString::String("web".into()), "/l"));
        spec(&mut p).containers[0].readiness_probe = Some(http(IntOrString::Int(9090), "/r"));
        spec(&mut p).containers[0].startup_probe =
            Some(http(IntOrString::String("nope".into()), "/s"));
        spec(&mut p).containers.push(Container {
            name: "side".into(),
            readiness_probe: Some(Probe {
                exec: Some(Default::default()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let probes = app_probes(&p);
        // An unresolvable name is left to fail as it would without mimic.
        assert_eq!(
            probes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["app/livez", "app/readyz"]
        );
        let AppProbe::Http {
            get,
            timeout_seconds,
        } = &probes["app/livez"]
        else {
            panic!("not an httpGet probe");
        };
        assert_eq!(get.port, IntOrString::Int(8080));
        assert_eq!(get.path.as_deref(), Some("/l"));
        assert_eq!(get.scheme.as_deref(), Some("HTTPS"));
        assert_eq!(get.host.as_deref(), Some("localhost"));
        assert_eq!(*timeout_seconds, Some(3));
        let AppProbe::Http { get, .. } = &probes["app/readyz"] else {
            panic!("not an httpGet probe");
        };
        assert_eq!(get.port, IntOrString::Int(9090));

        let doc = apply(&p);
        let value = doc["spec"]["initContainers"][1]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "MIMIC_APP_PROBES")
            .unwrap()["value"]
            .as_str()
            .unwrap();
        let parsed: BTreeMap<String, AppProbe> = serde_json::from_str(value).unwrap();
        assert!(value.contains("\"timeoutSeconds\":3"));
        assert_eq!(parsed, probes);
        let c = &doc["spec"]["containers"][0];
        for (k, kind) in [("livenessProbe", "livez"), ("readinessProbe", "readyz")] {
            assert_eq!(
                c[k]["httpGet"],
                json!({"path": format!("/app-health/app/{kind}"), "port": 15100, "scheme": "HTTP"})
            );
        }
        assert_eq!(c["startupProbe"]["httpGet"]["port"], "nope");
        assert_eq!(c["livenessProbe"]["periodSeconds"], 7);
        assert_eq!(
            doc["spec"]["containers"][1]["readinessProbe"],
            json!({"exec": {}})
        );
    }

    // ---- TLS trust wiring (TLS-11, TLS-12) ----

    fn container(name: &str, image: &str) -> Container {
        Container {
            name: name.into(),
            image: Some(image.into()),
            ..Default::default()
        }
    }

    fn env_of(c: &Value) -> BTreeMap<String, Value> {
        c["env"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| (e["name"].as_str().unwrap().to_string(), e.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    const JAVA: &str = "-Djavax.net.ssl.trustStore=/etc/mimic/tls/truststore.p12 -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=mimic";

    #[test]
    fn tls_11_volumes() {
        let doc = apply(&pod());
        assert_eq!(
            doc["spec"]["volumes"],
            json!([
                {"name": "mimic-trust", "emptyDir": {"sizeLimit": "16Mi"}},
                {"name": "mimic-token", "projected": {"sources": [{"serviceAccountToken": {
                    "audience": "mimic.platform.io", "expirationSeconds": 3600, "path": "token"
                }}]}}
            ])
        );
        // Added to the pod's own volumes when it has some.
        let mut p = pod();
        spec(&mut p).volumes = Some(vec![k8s_openapi::api::core::v1::Volume {
            name: "data".into(),
            ..Default::default()
        }]);
        let doc = apply(&p);
        let names: Vec<_> = doc["spec"]["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["data", "mimic-trust", "mimic-token"]);
    }

    #[test]
    fn tls_11_sidecar_mounts_and_variables() {
        let mut tls = wiring();
        tls.exclude = vec!["*.corp.example".into(), "api.example.com".into()];
        let doc = apply_with(&pod(), &tls);
        let sc = &doc["spec"]["initContainers"][1];
        assert_eq!(
            sc["volumeMounts"],
            json!([
                {"name": "mimic-trust", "mountPath": "/mimic"},
                {"name": "mimic-token", "mountPath": "/var/run/secrets/mimic.platform.io", "readOnly": true}
            ])
        );
        let env = env_of(sc);
        // `$` is doubled in literal values (INJ-21).
        assert_eq!(
            env["MIMIC_TLS_CA"]["value"],
            "-----BEGIN CERTIFICATE-----\nROOT $$x\n-----END CERTIFICATE-----\n"
        );
        assert_eq!(
            env["MIMIC_CONTROLLER_CA"]["value"],
            "-----BEGIN CERTIFICATE-----\nWEBHOOK\n-----END CERTIFICATE-----\n"
        );
        assert_eq!(
            env["MIMIC_CONTROLLER_URL"]["value"],
            "https://mimic.mimic-system.svc:8444"
        );
        assert_eq!(
            env["MIMIC_TLS_EXCLUDE"]["value"],
            "*.corp.example,api.example.com"
        );
    }

    #[test]
    fn dsc_20_redact_variable_is_the_joined_names_and_absent_without_any() {
        let mut tls = wiring();
        tls.redact = vec!["pin".into(), "x-tenant-key".into()];
        let doc = apply_with(&pod(), &tls);
        let env = env_of(&doc["spec"]["initContainers"][1]);
        assert_eq!(
            env["MIMIC_DISCOVERY_REDACT_NAMES"]["value"],
            "pin,x-tenant-key"
        );
        let doc = apply(&pod());
        let env = env_of(&doc["spec"]["initContainers"][1]);
        assert!(!env.contains_key("MIMIC_DISCOVERY_REDACT_NAMES"));
    }

    #[test]
    fn tls_12_exclude_variable_is_absent_without_exclusions() {
        let doc = apply(&pod());
        let env = env_of(&doc["spec"]["initContainers"][1]);
        assert!(!env.contains_key("MIMIC_TLS_EXCLUDE"));
        assert!(env.contains_key("MIMIC_TLS_CA"));
    }

    #[test]
    fn tls_11_one_trust_container_per_distinct_image_in_first_appearance_order() {
        let mut p = pod();
        {
            let s = spec(&mut p);
            s.init_containers = Some(vec![container("migrate", "db:1")]);
            s.containers = vec![
                container("app", "app:1"),
                container("proxy", "db:1"),
                container("worker", "app:1"),
            ];
            s.containers[1].image_pull_policy = Some("Always".into());
        }
        let doc = apply(&p);
        assert_eq!(
            init_names(&doc),
            [
                "mimic-init",
                "mimic-sidecar",
                "mimic-trust-0",
                "mimic-trust-1",
                "migrate"
            ]
        );
        let inits = doc["spec"]["initContainers"].as_array().unwrap();
        let (t0, t1) = (&inits[2], &inits[3]);
        assert_eq!(t0["image"], "db:1");
        assert_eq!(t1["image"], "app:1");
        // The pull policy of the first container running the image, none when unset.
        assert!(t0.get("imagePullPolicy").is_none());
        let mut p2 = p.clone();
        spec(&mut p2).init_containers.as_mut().unwrap()[0].image_pull_policy =
            Some("IfNotPresent".into());
        assert_eq!(
            apply(&p2)["spec"]["initContainers"][2]["imagePullPolicy"],
            "IfNotPresent"
        );
        for (n, t) in [t0, t1].into_iter().enumerate() {
            assert_eq!(t["command"], json!(["/mimic/bin/mimic-trust"]));
            assert_eq!(
                t["securityContext"],
                json!({
                    "runAsUser": 1337,
                    "runAsGroup": 1337,
                    "runAsNonRoot": true,
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]},
                    "seccompProfile": {"type": "RuntimeDefault"}
                })
            );
            assert_eq!(
                t["resources"],
                json!({"requests": {"cpu": "10m", "memory": "16Mi"}, "limits": {"memory": "64Mi"}})
            );
            assert_eq!(
                t["volumeMounts"],
                json!([{"name": "mimic-trust", "mountPath": "/mimic"}])
            );
            assert!(t.get("restartPolicy").is_none());
            let env = env_of(t);
            assert_eq!(env["MIMIC_TRUST_OUT"]["value"], format!("/mimic/trust/{n}"));
            assert_eq!(env["MIMIC_LOG"]["value"], "debug");
            assert!(
                env["MIMIC_TRUST_CA"]["value"]
                    .as_str()
                    .unwrap()
                    .contains("ROOT $$x")
            );
        }
        // Each app container and app init container mounts the bundle of its own image.
        let mount = |c: &Value| c["volumeMounts"].clone();
        let sub = |n: u8| {
            json!([{"name": "mimic-trust", "mountPath": "/etc/mimic/tls", "readOnly": true,
                "subPath": format!("trust/{n}")}])
        };
        assert_eq!(mount(&inits[4]), sub(0));
        let apps = doc["spec"]["containers"].as_array().unwrap();
        assert_eq!(mount(&apps[0]), sub(1));
        assert_eq!(mount(&apps[1]), sub(0));
        assert_eq!(mount(&apps[2]), sub(1));
    }

    #[test]
    fn tls_11_app_variables_and_the_ones_the_container_defines() {
        let mut p = pod();
        let var = |n: &str, v: &str| k8s_openapi::api::core::v1::EnvVar {
            name: n.into(),
            value: Some(v.into()),
            ..Default::default()
        };
        {
            let s = spec(&mut p);
            s.init_containers = Some(vec![container("migrate", "app:1")]);
            s.containers = vec![container("app", "app:1"), container("java", "app:1")];
            s.containers[0].env = Some(vec![var("SSL_CERT_FILE", "/mine.pem"), var("KEEP", "1")]);
            s.containers[1].env = Some(vec![var("JAVA_TOOL_OPTIONS", "-Xmx1g $(HOME)")]);
        }
        let doc = apply(&p);
        let app = env_of(&doc["spec"]["containers"][0]);
        assert_eq!(app["SSL_CERT_FILE"]["value"], "/mine.pem");
        assert_eq!(app["KEEP"]["value"], "1");
        assert_eq!(
            app["REQUESTS_CA_BUNDLE"]["value"],
            "/etc/mimic/tls/ca-bundle.pem"
        );
        assert_eq!(
            app["CURL_CA_BUNDLE"]["value"],
            "/etc/mimic/tls/ca-bundle.pem"
        );
        assert_eq!(
            app["NODE_EXTRA_CA_CERTS"]["value"],
            "/etc/mimic/tls/node-extra-ca.pem"
        );
        assert_eq!(app["JAVA_TOOL_OPTIONS"]["value"], JAVA);
        // The app's own options come later; `$` is not touched (the app wrote them).
        let java = env_of(&doc["spec"]["containers"][1]);
        assert_eq!(
            java["JAVA_TOOL_OPTIONS"]["value"],
            format!("{JAVA} -Xmx1g $(HOME)")
        );
        assert_eq!(
            java["SSL_CERT_FILE"]["value"],
            "/etc/mimic/tls/ca-bundle.pem"
        );
        // App init containers get the wiring too.
        let migrate = env_of(&doc["spec"]["initContainers"][3]);
        assert_eq!(migrate["JAVA_TOOL_OPTIONS"]["value"], JAVA);
    }

    #[test]
    fn tls_11_value_from_java_options_are_untouched() {
        let mut p = pod();
        spec(&mut p).containers[0].env = Some(vec![k8s_openapi::api::core::v1::EnvVar {
            name: "JAVA_TOOL_OPTIONS".into(),
            value_from: Some(Default::default()),
            ..Default::default()
        }]);
        let doc = apply(&p);
        let env = &doc["spec"]["containers"][0]["env"];
        let java: Vec<_> = env
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "JAVA_TOOL_OPTIONS")
            .collect();
        assert_eq!(java.len(), 1);
        assert!(java[0].get("value").is_none());
    }

    #[test]
    fn tls_11_existing_mounts_are_kept_and_app_has_no_token() {
        let mut p = pod();
        spec(&mut p).containers[0].volume_mounts =
            Some(vec![k8s_openapi::api::core::v1::VolumeMount {
                name: "data".into(),
                mount_path: "/data".into(),
                ..Default::default()
            }]);
        let doc = apply(&p);
        let mounts = doc["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0]["name"], "data");
        assert!(
            !doc["spec"]["containers"]
                .to_string()
                .contains("mimic-token")
        );
        assert!(
            !doc["spec"]["initContainers"][0]
                .to_string()
                .contains("mimic-trust")
        );
    }

    #[test]
    fn tls_11_patch_is_deterministic() {
        let mut p = pod();
        spec(&mut p).containers.push(container("b", "b:1"));
        let a = serde_json::to_vec(&patch(&p, "shop-api", &cfg(), &wiring()).unwrap()).unwrap();
        let b = serde_json::to_vec(&patch(&p, "shop-api", &cfg(), &wiring()).unwrap()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tls_11_name_conflict() {
        use k8s_openapi::api::core::v1::{Volume, VolumeMount};
        let mut cases: Vec<Pod> = Vec::new();
        for n in ["mimic-trust-0", "mimic-trust-12"] {
            let mut p = pod();
            spec(&mut p).containers.push(container(n, "x"));
            cases.push(p);
        }
        for v in ["mimic-trust", "mimic-token"] {
            let mut p = pod();
            spec(&mut p).volumes = Some(vec![Volume {
                name: v.into(),
                ..Default::default()
            }]);
            cases.push(p);
        }
        for path in ["/etc/mimic/tls", "/etc/mimic/tls/"] {
            let mut p = pod();
            spec(&mut p).containers[0].volume_mounts = Some(vec![VolumeMount {
                name: "data".into(),
                mount_path: path.into(),
                ..Default::default()
            }]);
            cases.push(p);
        }
        for p in &cases {
            assert_eq!(
                run(p, NamespaceMode::Enabled, false),
                skip(SkipReason::NameConflict)
            );
        }
        // Other names and paths are fine.
        let mut p = pod();
        spec(&mut p)
            .containers
            .push(container("mimic-trusted", "x"));
        spec(&mut p)
            .containers
            .push(container("mimic-trust-x", "x"));
        spec(&mut p).containers[0].volume_mounts = Some(vec![VolumeMount {
            name: "data".into(),
            mount_path: "/etc/mimic/tls-other".into(),
            ..Default::default()
        }]);
        assert_eq!(run(&p, NamespaceMode::Enabled, false), injected());
    }
}
