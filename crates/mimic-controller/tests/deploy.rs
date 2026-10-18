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
