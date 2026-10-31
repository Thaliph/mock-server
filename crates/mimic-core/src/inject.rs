//! Injection decision (spec §7.1): which pods get the sidecar, which are skipped and why.

use k8s_openapi::api::core::v1::Pod;

/// Label key read on namespaces and pods.
pub const INJECT_LABEL: &str = "mimic.platform.io/inject";
/// Name of the injected sidecar init container, also the idempotence marker.
pub const SIDECAR_NAME: &str = "mimic-sidecar";
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
pub fn deployment_of(_pod: &Pod) -> Option<String> {
    todo!()
}

/// Decides what to do with `pod` created in `namespace`. `workload_exists` tells whether a
/// `MimicWorkload` of that namespace has `spec.deployment` equal to the given name.
pub fn decide(
    _pod: &Pod,
    _namespace: &str,
    _controller_namespace: &str,
    _namespace_mode: NamespaceMode,
    _workload_exists: impl Fn(&str) -> bool,
) -> Decision {
    todo!()
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
}
