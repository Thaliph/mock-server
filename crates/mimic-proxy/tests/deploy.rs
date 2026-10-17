//! Les manifests d'exemple restent valides pour la configuration du sidecar.

use mimic_proxy::config::{Config, Mode};

#[test]
fn example_configmap_is_a_valid_config() {
    let manifest: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../../deploy/example/configmap.yaml")).unwrap();
    let text = manifest["data"]["mimic.yaml"].as_str().unwrap();
    let mut config: Config = serde_yaml::from_str(text).unwrap();
    config
        .apply_env(|k| match k {
            "POD_NAME" => Some("checkout-7d9f-abcde".into()),
            "POD_NAMESPACE" => Some("shop".into()),
            "MIMIC_WORKLOAD" => Some("checkout".into()),
            _ => None,
        })
        .unwrap();
    config.validate().unwrap();
    assert_eq!(config.mode, Mode::Record);
    assert!(config.share.is_some());
}
