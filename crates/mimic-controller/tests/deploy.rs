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
