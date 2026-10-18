//! Les manifests du contrôleur restent cohérents avec le code.

use mimic_controller::config::{AuthConfig, ControllerConfig};

#[test]
fn crd_manifest_matches_the_code() {
    let file = include_str!("../../../deploy/controller/crd.yaml");
    let body: String = file
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(
        body,
        mimic_controller::crd::crd_yaml(),
        "régénérer : mimic-controller crd > deploy/controller/crd.yaml"
    );
}

#[test]
fn controller_configmap_is_valid() {
    let docs = include_str!("../../../deploy/controller/controller.yaml");
    let cm: serde_yaml::Value = serde_yaml::Deserializer::from_str(docs)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .find(|d: &serde_yaml::Value| d["kind"] == "ConfigMap")
        .unwrap();
    let config: ControllerConfig =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    assert!(matches!(config.auth, AuthConfig::Kubernetes));
    assert!(config.ui_listen.ip().is_loopback());
}

fn docs(text: &str) -> Vec<serde_yaml::Value> {
    serde_yaml::Deserializer::from_str(text)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect()
}

/// Le contrôleur lit les labels des namespaces (garde-fou production, CRD-07).
#[test]
fn crd_07_controller_may_watch_namespaces() {
    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));
    let role = rbac
        .iter()
        .find(|d| d["kind"] == "ClusterRole" && d["metadata"]["name"] == "mimic-controller")
        .unwrap();
    let rule = role["rules"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|r| r["resources"][0] == "namespaces")
        .expect("droit sur les namespaces");
    for verb in ["get", "list", "watch"] {
        assert!(
            rule["verbs"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|v| v == verb),
            "{verb}"
        );
    }
}

/// La politique d'admission refuse les mêmes modes, sur le même label, que le contrôleur.
#[test]
fn crd_07_admission_policy_matches_the_guard() {
    use mimic_controller::reconcile::{PRODUCTION_LABEL, is_mock};
    let policy = docs(include_str!(
        "../../../deploy/controller/production-guard.yaml"
    ));
    let vap = policy
        .iter()
        .find(|d| d["kind"] == "ValidatingAdmissionPolicy")
        .unwrap();
    let expr = vap["spec"]["validations"][0]["expression"]
        .as_str()
        .unwrap();
    for mode in mimic_controller::crd::MODES {
        assert_eq!(expr.contains(&format!("'{mode}'")), is_mock(mode), "{mode}");
    }
    assert!(expr.contains(&format!("'{}'", PRODUCTION_LABEL.0)));
    assert!(expr.contains(&format!("'{}'", PRODUCTION_LABEL.1)));
    let rule = &vap["spec"]["matchConstraints"]["resourceRules"][0];
    assert_eq!(rule["resources"][0], "mimicworkloads");
    let binding = policy
        .iter()
        .find(|d| d["kind"] == "ValidatingAdmissionPolicyBinding")
        .unwrap();
    assert_eq!(binding["spec"]["policyName"], vap["metadata"]["name"]);
}

/// Historique court par défaut (CTL-21) : 24 heures, dans le code comme dans le manifest.
#[test]
fn ctl_21_retention_defaults_to_24_hours() {
    let day = std::time::Duration::from_secs(24 * 3600);
    let defaults = ControllerConfig::default();
    assert_eq!(defaults.retention, day);
    assert_eq!(defaults.max_exemplars_per_workload, 10_000);
    assert_eq!(defaults.max_exemplar_bytes_per_workload, 64 << 20);
    assert_eq!(defaults.max_exemplar_bytes_total, 512 << 20);
    let cm = docs(include_str!("../../../deploy/controller/controller.yaml"))
        .into_iter()
        .find(|d| d["kind"] == "ConfigMap")
        .unwrap();
    let text = cm["data"]["controller.yaml"].as_str().unwrap();
    let raw: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
    assert_eq!(raw["retention"].as_u64(), Some(86_400));
    assert_eq!(raw["max_exemplars_per_workload"].as_u64(), Some(10_000));
    assert_eq!(
        raw["max_exemplar_bytes_per_workload"].as_u64(),
        Some(64 << 20)
    );
    assert_eq!(raw["max_exemplar_bytes_total"].as_u64(), Some(512 << 20));
    let config: ControllerConfig = serde_yaml::from_str(text).unwrap();
    assert_eq!(config.retention, day);
}

fn find<'a>(docs: &'a [serde_yaml::Value], kind: &str, name: &str) -> &'a serde_yaml::Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("{kind} {name} absent"))
}

fn verbs(rule: &serde_yaml::Value) -> Vec<String> {
    rule["verbs"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn rule_on<'a>(role: &'a serde_yaml::Value, resource: &str) -> Option<&'a serde_yaml::Value> {
    role["rules"].as_sequence().unwrap().iter().find(|r| {
        r["resources"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|x| x == resource)
    })
}

/// Haute disponibilité (CTL-28, spec §5.5.4) : deux réplicas, une seule routée.
#[test]
fn ctl_28_controller_runs_two_replicas_behind_the_active_one() {
    use mimic_controller::leader::LEADER_LABEL;
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let deploy = find(&manifests, "Deployment", "mimic-controller");
    let spec = &deploy["spec"];
    assert_eq!(spec["replicas"].as_u64(), Some(2));
    assert_eq!(spec["strategy"]["type"], "RollingUpdate");
    assert_eq!(
        spec["strategy"]["rollingUpdate"]["maxUnavailable"].as_u64(),
        Some(0)
    );
    let pod = &spec["template"]["spec"];
    let anti =
        &pod["affinity"]["podAntiAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"][0];
    assert_eq!(
        anti["podAffinityTerm"]["topologyKey"],
        "kubernetes.io/hostname"
    );
    assert!(
        pod["affinity"]["podAntiAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
            .is_null(),
        "anti-affinité préférentielle : un nœud unique reste possible"
    );
    let container = &pod["containers"][0];
    let env = container["env"].as_sequence().expect("env du contrôleur");
    let pod_name = env
        .iter()
        .find(|e| e["name"] == "POD_NAME")
        .expect("POD_NAME");
    assert_eq!(
        pod_name["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.name"
    );
    assert!(container["livenessProbe"]["httpGet"]["path"].is_string());

    // Le Service ne route que vers la réplique marquée.
    let svc = find(&manifests, "Service", "mimic-controller");
    assert_eq!(svc["spec"]["selector"][LEADER_LABEL.0], LEADER_LABEL.1);
    assert_eq!(
        svc["spec"]["selector"]["app.kubernetes.io/name"],
        "mimic-controller"
    );

    let pdb = find(&manifests, "PodDisruptionBudget", "mimic-controller");
    assert_eq!(pdb["spec"]["minAvailable"].as_u64(), Some(1));
    assert_eq!(
        pdb["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"],
        "mimic-controller"
    );

    let cm = find(&manifests, "ConfigMap", "mimic-controller");
    let config: ControllerConfig =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    let election = config.leader_election.expect("leader_election");
    assert_eq!(election.lease, "mimic-controller");
    assert_eq!(election.namespace, "mimic-system");
    assert_eq!(election.identity, None, "identité lue dans POD_NAME");

    // Droits : bail et marque dans mimic-system seulement.
    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));
    let role = find(&rbac, "Role", "mimic-controller");
    assert_eq!(role["metadata"]["namespace"], "mimic-system");
    let leases = rule_on(role, "leases").expect("droit sur les leases");
    assert_eq!(leases["apiGroups"][0], "coordination.k8s.io");
    let lease_verbs: Vec<String> = role["rules"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter(|r| {
            r["resources"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|x| x == "leases")
        })
        .flat_map(verbs)
        .collect();
    for verb in ["get", "create", "update"] {
        assert!(lease_verbs.iter().any(|v| v == verb), "leases : {verb}");
    }
    let pods = rule_on(role, "pods").expect("droit sur les pods");
    for verb in ["list", "patch"] {
        assert!(verbs(pods).iter().any(|v| v == verb), "pods : {verb}");
    }
    let binding = find(&rbac, "RoleBinding", "mimic-controller");
    assert_eq!(binding["metadata"]["namespace"], "mimic-system");
    assert_eq!(binding["roleRef"]["kind"], "Role");
    assert_eq!(binding["subjects"][0]["name"], "mimic-controller");
    let cluster = find(&rbac, "ClusterRole", "mimic-controller");
    assert!(rule_on(cluster, "leases").is_none());
    let cluster_pods = rule_on(cluster, "pods").unwrap();
    assert_eq!(
        verbs(cluster_pods),
        ["get"],
        "aucune écriture sur les pods du cluster"
    );
}
