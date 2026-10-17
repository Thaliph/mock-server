//! Le manifest du webhook reste cohérent avec le code (spec §9.4).

use mimic_injector::config::InjectorConfig;
use mimic_injector::inject::{CA_DIR, CONTROLLER_CA_DIR, STORE_DIR, TOKEN_DIR};
use mimic_proxy::config::Config;

fn docs() -> Vec<serde_yaml::Value> {
    serde_yaml::Deserializer::from_str(include_str!("../../../deploy/injector/injector.yaml"))
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect()
}

fn kind(kind: &str) -> serde_yaml::Value {
    docs().into_iter().find(|d| d["kind"] == kind).unwrap()
}

#[test]
fn injector_configmap_is_valid_and_matches_the_mounts() {
    let cm = kind("ConfigMap");
    let config: InjectorConfig =
        serde_yaml::from_str(cm["data"]["injector.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    assert!(config.tls.is_some());

    let sidecar = Config::load_with(None, |k| match k {
        "MIMIC_CONFIG_YAML" => Some(config.sidecar_config.clone()),
        "POD_NAME" => Some("checkout-7d9f-abcde".into()),
        "POD_NAMESPACE" => Some("shop".into()),
        "MIMIC_WORKLOAD" => Some("checkout".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        sidecar.store.dir.as_deref(),
        Some(std::path::Path::new(STORE_DIR))
    );
    assert_eq!(
        sidecar.tls.ca_dir.as_deref(),
        Some(std::path::Path::new(CA_DIR))
    );
    let share = sidecar.share.unwrap();
    assert!(share.ca_file.unwrap().starts_with(CONTROLLER_CA_DIR));
    assert!(share.token_file.unwrap().starts_with(TOKEN_DIR));
    // Les probes du sidecar passent par l'IP du pod.
    assert!(!sidecar.admin.listen.ip().is_loopback());
}

#[test]
fn webhook_never_blocks_pod_creation() {
    let hook = &kind("MutatingWebhookConfiguration")["webhooks"][0];
    assert_eq!(hook["failurePolicy"], "Ignore");
    assert_eq!(hook["sideEffects"], "None");
    assert_eq!(
        hook["namespaceSelector"]["matchLabels"]["mimic.platform.io/enabled"],
        "true"
    );
    assert_eq!(hook["clientConfig"]["service"]["path"], "/mutate");
}
