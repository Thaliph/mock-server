//! Le manifest du webhook reste cohérent avec le code (spec §5.5.5, §9.4).

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

#[test]
fn inj_12_webhook_is_called_again_after_the_mesh_webhook() {
    let hook = &kind("MutatingWebhookConfiguration")["webhooks"][0];
    assert_eq!(hook["reinvocationPolicy"], "IfNeeded");
}

/// `512Mi`, `256Mi`, `1Gi` → octets.
fn quantity(q: &str) -> u64 {
    let (n, unit) = q.split_at(q.find(|c: char| !c.is_ascii_digit()).unwrap_or(q.len()));
    let n: u64 = n.parse().unwrap();
    n * match unit {
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "" => 1,
        other => panic!("unité {other}"),
    }
}

#[test]
fn cap_08_default_limits_fit_in_the_injected_pod() {
    use k8s_openapi::api::core::v1::Pod;
    use mimic_injector::inject::{Outcome, mutate};

    let cm = kind("ConfigMap");
    let config: InjectorConfig =
        serde_yaml::from_str(cm["data"]["injector.yaml"].as_str().unwrap()).unwrap();
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {"name": "checkout-1", "labels": {"app.kubernetes.io/name": "checkout"}},
        "spec": {"containers": [{"name": "app", "image": "app:1"}]}
    }))
    .unwrap();
    let Outcome::Injected(pod) = mutate(&pod, &config) else {
        panic!("pod non injecté");
    };
    let spec = pod.spec.unwrap();
    let sidecar = spec
        .init_containers
        .unwrap()
        .into_iter()
        .find(|c| c.name == "mimic")
        .unwrap();
    let memory = quantity(&sidecar.resources.unwrap().limits.unwrap()["memory"].0);
    let store_volume = spec
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "mimic-store")
        .unwrap();
    let disk = quantity(&store_volume.empty_dir.unwrap().size_limit.unwrap().0);

    let sidecar = Config::load_with(None, |k| match k {
        "MIMIC_CONFIG_YAML" => Some(config.sidecar_config.clone()),
        "MIMIC_WORKLOAD" => Some("checkout".into()),
        _ => None,
    })
    .unwrap();
    // Le journal local laisse les trois quarts de l'emptyDir aux snapshots imposés.
    let journal = sidecar.store.segment_bytes * sidecar.store.max_segments as u64;
    assert!(
        journal <= disk / 4,
        "journal {journal} pour un emptyDir de {disk}"
    );
    // Store et file de partage : un tiers de la limite, le reste pour le moteur de mock,
    // la découverte et les bodies en vol.
    let share = sidecar.share.map_or(0, |s| s.max_backlog_bytes) as u64;
    let store = sidecar.capture.max_store_bytes as u64;
    assert!(
        store + share <= memory / 3,
        "store {store} + partage {share} pour une limite de {memory}"
    );
}

/// Manifests de production (INJ-20).
#[test]
fn inj_20_production_manifests() {
    const IMAGE: &str = "ghcr.io/thaliph/mimic:0.1.0";
    for (name, text) in [
        (
            "injector.yaml",
            include_str!("../../../deploy/injector/injector.yaml"),
        ),
        (
            "controller.yaml",
            include_str!("../../../deploy/controller/controller.yaml"),
        ),
        (
            "deployment.yaml",
            include_str!("../../../deploy/example/deployment.yaml"),
        ),
    ] {
        assert!(
            !text.contains("registry.example.com/platform"),
            "{name} : image factice"
        );
        assert!(text.contains(IMAGE), "{name} : {IMAGE}");
    }

    let cm = kind("ConfigMap");
    let config: InjectorConfig =
        serde_yaml::from_str(cm["data"]["injector.yaml"].as_str().unwrap()).unwrap();
    assert_eq!(config.image, IMAGE);
    assert!(
        config.exclude_outbound_cidrs.is_empty(),
        "IP de l'API Kubernetes lue au démarrage, rien de codé en dur : {:?}",
        config.exclude_outbound_cidrs
    );

    let deploy = kind("Deployment");
    let pod = &deploy["spec"]["template"]["spec"];
    assert_eq!(pod["containers"][0]["image"], IMAGE);
    assert_eq!(pod["securityContext"]["runAsUser"].as_u64(), Some(1338));
    assert_eq!(pod["securityContext"]["runAsGroup"].as_u64(), Some(1338));
    let anti =
        &pod["affinity"]["podAntiAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"][0];
    assert_eq!(
        anti["podAffinityTerm"]["topologyKey"],
        "kubernetes.io/hostname"
    );
    assert_eq!(
        anti["podAffinityTerm"]["labelSelector"]["matchLabels"]["app.kubernetes.io/name"],
        "mimic-injector"
    );
    assert_eq!(pod["priorityClassName"], "mimic-control-plane");
    let classes: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(include_str!(
        "../../../deploy/controller/namespace.yaml"
    ))
    .map(|d| serde::Deserialize::deserialize(d).unwrap())
    .collect();
    assert!(
        classes
            .iter()
            .any(|d| d["kind"] == "PriorityClass" && d["metadata"]["name"] == "mimic-control-plane"),
        "PriorityClass fournie"
    );

    let hook = &kind("MutatingWebhookConfiguration")["webhooks"][0];
    let selector = &hook["namespaceSelector"];
    assert_eq!(selector["matchLabels"]["mimic.platform.io/enabled"], "true");
    let excluded = selector["matchExpressions"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "kubernetes.io/metadata.name")
        .expect("exclusion par nom de namespace");
    assert_eq!(excluded["operator"], "NotIn");
    let names: Vec<&str> = excluded["values"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&"kube-system") && names.contains(&"mimic-system"));
}

/// Barrière de démarrage dans les manifests (CTL-41, INJ-21, spec §5.5.5) : `share.startup`
/// explicite, et `startupProbe` du sidecar écrit à la main cohérente avec son délai.
#[test]
fn ctl_41_inj_21_manifests_give_the_startup_gate() {
    use mimic_proxy::config::StartupFallback;
    let cm = kind("ConfigMap");
    let config: InjectorConfig =
        serde_yaml::from_str(cm["data"]["injector.yaml"].as_str().unwrap()).unwrap();
    let raw: serde_yaml::Value = serde_yaml::from_str(&config.sidecar_config).unwrap();
    assert_eq!(raw["share"]["startup"]["timeout"].as_u64(), Some(30));
    assert_eq!(raw["share"]["startup"]["fallback"], "record");

    let example: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../../deploy/example/configmap.yaml")).unwrap();
    let text = example["data"]["mimic.yaml"].as_str().unwrap();
    let raw: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
    assert_eq!(raw["share"]["startup"]["fallback"], "record");
    let sidecar: Config = serde_yaml::from_str(text).unwrap();
    let startup = sidecar.share.unwrap().startup;
    assert_eq!(startup.fallback, StartupFallback::Record);

    let deployment: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../../deploy/example/deployment.yaml")).unwrap();
    let inits = deployment["spec"]["template"]["spec"]["initContainers"]
        .as_sequence()
        .unwrap();
    let mimic = inits.iter().find(|c| c["name"] == "mimic").unwrap();
    let probe = &mimic["startupProbe"];
    assert_eq!(probe["httpGet"]["path"], "/readyz");
    assert_eq!(probe["periodSeconds"].as_u64(), Some(1));
    assert_eq!(
        probe["failureThreshold"].as_u64(),
        Some(startup.timeout.as_secs() + 30),
        "fenêtre de la sonde : share.startup.timeout + 30 s"
    );
    assert_eq!(mimic["readinessProbe"]["httpGet"]["path"], "/readyz");
    assert_eq!(mimic["livenessProbe"]["httpGet"]["path"], "/healthz");
}
