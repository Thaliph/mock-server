//! Injection decision (spec §7.1): which pods get the sidecar, which are skipped and why.

use k8s_openapi::api::core::v1::{Container, HTTPGetAction, Pod, Probe, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

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
    Decision::Inject { deployment }
}

/// One RFC 6902 operation of the injection patch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JsonPatchOp {
    pub op: &'static str,
    pub path: String,
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
}

/// Injection patch of `pod`, owned by `deployment`.
pub fn patch(
    pod: &Pod,
    deployment: &str,
    cfg: &InjectConfig,
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
        "env": [{"name": "MIMIC_EXCLUDE_INBOUND_PORTS", "value": exclusions}],
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
    let sidecar = json!({
        "name": SIDECAR_NAME,
        "image": cfg.sidecar_image,
        "command": ["/usr/local/bin/mimic-sidecar"],
        "restartPolicy": "Always",
        "env": [
            {"name": "MIMIC_WORKLOAD", "value": deployment},
            field("MIMIC_POD_NAMESPACE", "metadata.namespace"),
            field("MIMIC_POD_NAME", "metadata.name"),
            field("MIMIC_POD_IP", "status.podIP"),
            {"name": "MIMIC_APP_PROBES", "value": serde_json::to_string(&rewritten)?},
            {"name": "MIMIC_LOG", "value": cfg.sidecar_log},
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
    if spec.is_some_and(|s| s.init_containers.as_ref().is_some_and(|c| !c.is_empty())) {
        ops.push(add("/spec/initContainers/0".into(), init));
        ops.push(add("/spec/initContainers/1".into(), sidecar));
    } else {
        ops.push(add("/spec/initContainers".into(), json!([init, sidecar])));
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
        for (field, kind, _) in probes_of(c) {
            if rewritten.contains_key(&format!("{}/{kind}", c.name)) {
                ops.push(JsonPatchOp {
                    op: "replace",
                    path: format!("/spec/containers/{i}/{field}/httpGet"),
                    value: json!({
                        "path": format!("/app-health/{}/{kind}", c.name),
                        "port": ADMIN_PORT,
                        "scheme": "HTTP",
                    }),
                });
            }
        }
    }
    Ok(ops)
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

/// Inbound ports `mimic-init` must not redirect (spec §4), sorted and deduplicated.
pub fn excluded_inbound_ports(pod: &Pod) -> Vec<u16> {
    let mut ports = BTreeSet::from([ADMIN_PORT as u16]);
    for c in pod.spec.iter().flat_map(|s| &s.containers) {
        for (_, _, probe) in probes_of(c) {
            let Some(probe) = probe else { continue };
            let tcp = probe.tcp_socket.as_ref().and_then(|t| resolve(c, &t.port));
            let grpc = probe.grpc.as_ref().map(|g| g.port);
            ports.extend(
                tcp.into_iter()
                    .chain(grpc)
                    .filter_map(|p| u16::try_from(p).ok())
                    .filter(|p| *p != 0),
            );
        }
    }
    ports.into_iter().collect()
}

/// One original `httpGet` probe as handed to the sidecar in `MIMIC_APP_PROBES`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProbe {
    #[serde(flatten)]
    pub get: HTTPGetAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<i32>,
}

/// Original `httpGet` probes of the app containers, keyed `<container>/<livez|readyz|startupz>`,
/// named ports resolved; a probe whose named port does not resolve is left out (and not
/// rewritten), so it fails as it would without mimic.
pub fn app_probes(pod: &Pod) -> BTreeMap<String, AppProbe> {
    let mut out = BTreeMap::new();
    for c in pod.spec.iter().flat_map(|s| &s.containers) {
        for (_, kind, probe) in probes_of(c) {
            let Some((probe, mut get)) = probe.and_then(|p| Some((p, p.http_get.clone()?))) else {
                continue;
            };
            let Some(port) = resolve(c, &get.port) else {
                continue;
            };
            get.port = IntOrString::Int(port);
            let timeout_seconds = probe.timeout_seconds;
            out.insert(
                format!("{}/{kind}", c.name),
                AppProbe {
                    get,
                    timeout_seconds,
                },
            );
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
        }
    }

    fn apply(p: &Pod) -> Value {
        let ops = patch(p, "shop-api", &cfg()).unwrap();
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
        assert_eq!(init_names(&apply(&pod())), ["mimic-init", "mimic-sidecar"]);
        let mut p = pod();
        spec(&mut p).init_containers = Some(vec![Container {
            name: "migrate".into(),
            ..Default::default()
        }]);
        assert_eq!(
            init_names(&apply(&p)),
            ["mimic-init", "mimic-sidecar", "migrate"]
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
        let a = serde_json::to_string(&patch(&p, "shop-api", &cfg()).unwrap()).unwrap();
        let b = serde_json::to_string(&patch(&p, "shop-api", &cfg()).unwrap()).unwrap();
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
        assert!(sc.get("volumeMounts").is_none());
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
            c.liveness_probe = Some(tcp(IntOrString::Int(9000)));
        }
        let mut expected = serde_json::to_value(&p).unwrap();
        expected["spec"]["containers"][0]["readinessProbe"]["httpGet"] =
            json!({"path": "/app-health/app/readyz", "port": 15100, "scheme": "HTTP"});
        // Everything else is the original, init containers and annotation aside.
        let mut after = apply(&p);
        after["spec"]
            .as_object_mut()
            .unwrap()
            .remove("initContainers");
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
    fn inj_04_exclusions_from_tcp_and_grpc_probes() {
        let mut p = pod();
        spec(&mut p).containers[0].ports = Some(vec![named("db", 5432), named("web", 8080)]);
        spec(&mut p).containers[0].liveness_probe = Some(tcp(IntOrString::String("db".into())));
        spec(&mut p).containers[0].readiness_probe = Some(tcp(IntOrString::Int(5432)));
        spec(&mut p).containers[0].startup_probe = Some(grpc(50051));
        spec(&mut p).containers.push(Container {
            name: "other".into(),
            // Names are resolved against the probe's own container: "db" and "nope" are unknown.
            liveness_probe: Some(tcp(IntOrString::String("db".into()))),
            readiness_probe: Some(tcp(IntOrString::String("nope".into()))),
            startup_probe: Some(tcp(IntOrString::Int(81))),
            ..Default::default()
        });
        assert_eq!(excluded_inbound_ports(&p), vec![81, 5432, 15100, 50051]);
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
    fn inj_04_exclusions_reach_mimic_init() {
        let mut p = pod();
        spec(&mut p).containers[0].liveness_probe = Some(tcp(IntOrString::Int(9000)));
        let doc = apply(&p);
        assert_eq!(
            doc["spec"]["initContainers"][0]["env"],
            json!([{"name": "MIMIC_EXCLUDE_INBOUND_PORTS", "value": "9000,15100"}])
        );
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
            readiness_probe: Some(tcp(IntOrString::Int(1))),
            ..Default::default()
        });
        let probes = app_probes(&p);
        // An unresolvable name is left to fail as it would without mimic.
        assert_eq!(
            probes.keys().map(String::as_str).collect::<Vec<_>>(),
            ["app/livez", "app/readyz"]
        );
        let live = &probes["app/livez"];
        assert_eq!(live.get.port, IntOrString::Int(8080));
        assert_eq!(live.get.path.as_deref(), Some("/l"));
        assert_eq!(live.get.scheme.as_deref(), Some("HTTPS"));
        assert_eq!(live.get.host.as_deref(), Some("localhost"));
        assert_eq!(live.timeout_seconds, Some(3));
        assert_eq!(probes["app/readyz"].get.port, IntOrString::Int(9090));

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
            json!({"tcpSocket": {"port": 1}})
        );
    }
}
