//! Injection en production (spec §9.4) : IP de l'API Kubernetes exclue d'office et IPv6
//! (INJ-17), annotations validées avant d'atteindre iptables (INJ-18), Pod Security et CA
//! facultative (INJ-19).

use k8s_openapi::api::core::v1::{Container, Pod};
use mimic_injector::config::InjectorConfig;
use mimic_injector::inject::{Outcome, mutate, warnings};
use serde_json::json;

const YAML: &str = r#"
image: ghcr.io/thaliph/mimic:0.1.0
exclude_outbound_cidrs: [10.0.0.0/8]
sidecar_config: |
  mode: record
"#;

fn config_with(yaml: &str, api_host: Option<&str>) -> InjectorConfig {
    InjectorConfig::parse(yaml, |k| match k {
        "KUBERNETES_SERVICE_HOST" => api_host.map(String::from),
        _ => None,
    })
    .unwrap()
}

fn pod(annotations: serde_json::Value) -> Pod {
    serde_json::from_value(json!({
        "metadata": {
            "name": "checkout-1",
            "labels": {"app.kubernetes.io/name": "checkout"},
            "annotations": annotations,
        },
        "spec": {"containers": [{
            "name": "app", "image": "app:1",
            "ports": [{"containerPort": 8080}, {"containerPort": 9090}]
        }]}
    }))
    .unwrap()
}

fn init_of(pod: &Pod, config: &InjectorConfig) -> (Container, Pod) {
    let Outcome::Injected(p) = mutate(pod, config) else {
        panic!("non injecté");
    };
    let init = p.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0].clone();
    assert_eq!(init.name, "mimic-init");
    (init, *p)
}

fn env<'a>(c: &'a Container, name: &str) -> Option<&'a str> {
    c.env
        .iter()
        .flatten()
        .find(|e| e.name == name)
        .and_then(|e| e.value.as_deref())
}

#[test]
fn inj_17_kubernetes_api_is_excluded_without_configuration() {
    let config = config_with(YAML, Some("10.100.0.1"));
    let p =
        pod(json!({"mimic.platform.io/exclude-outbound-cidrs": "10.100.0.1/32,192.168.0.0/16"}));
    let (init, _) = init_of(&p, &config);
    assert_eq!(
        env(&init, "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
        Some("10.100.0.1/32,10.0.0.0/8,192.168.0.0/16")
    );
    assert_eq!(env(&init, "MIMIC_IPV6"), None);

    let config = config_with(YAML, Some("fd00::1"));
    let (init, _) = init_of(&pod(json!({})), &config);
    assert_eq!(
        env(&init, "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
        Some("fd00::1/128,10.0.0.0/8")
    );

    // Hors cluster : rien d'ajouté ; valeur qui n'est pas une IP : ignorée.
    for host in [None, Some("kubernetes.default.svc")] {
        let config = config_with(YAML, host);
        let (init, _) = init_of(&pod(json!({})), &config);
        assert_eq!(
            env(&init, "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
            Some("10.0.0.0/8")
        );
    }

    let config = config_with(&format!("{YAML}ipv6: true\n"), Some("10.100.0.1"));
    let (init, _) = init_of(&pod(json!({})), &config);
    assert_eq!(env(&init, "MIMIC_IPV6"), Some("true"));
}

#[test]
fn inj_17_invalid_configured_cidr_is_refused_at_startup() {
    let yaml = YAML.replace("10.0.0.0/8", "10.0.0.0/33");
    let err = InjectorConfig::parse(&yaml, |_| None).unwrap_err();
    assert!(err.contains("exclude_outbound_cidrs"), "{err}");
}

#[test]
fn inj_18_invalid_annotation_values_never_reach_iptables() {
    let config = config_with(YAML, None);
    let p = pod(json!({
        "mimic.platform.io/inbound-ports": "8080,http,70000",
        "mimic.platform.io/exclude-outbound-cidrs": "10.1.0.0/16,bad,10.1.0.0/33,10.2.0.0/16\n-A OUTPUT -j ACCEPT",
        "mimic.platform.io/exclude-outbound-ports": "5432,x",
    }));
    let (init, injected) = init_of(&p, &config);
    assert_eq!(env(&init, "MIMIC_INBOUND_PORTS"), Some("8080"));
    assert_eq!(
        env(&init, "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
        Some("10.0.0.0/8,10.1.0.0/16")
    );
    assert_eq!(env(&init, "MIMIC_EXCLUDE_OUTBOUND_PORTS"), Some("5432"));
    let all = serde_json::to_string(&injected.spec).unwrap();
    for bad in ["70000", "bad", "/33", "-A OUTPUT"] {
        assert!(!all.contains(bad), "{bad} dans le pod injecté");
    }
    let w = warnings(&p);
    assert_eq!(w.len(), 6, "{w:?}");
    assert!(
        w.iter()
            .any(|w| w.contains("inbound-ports") && w.contains("http"))
    );
    assert!(
        w.iter()
            .any(|w| w.contains("exclude-outbound-cidrs") && w.contains("bad"))
    );
    assert!(
        w.iter()
            .any(|w| w.contains("exclude-outbound-ports") && w.contains("\"x\""))
    );
    assert!(w.iter().all(|w| !w.contains('\n')), "{w:?}");

    // Aucun port valide : ports déclarés, avec un avertissement.
    let p = pod(json!({"mimic.platform.io/inbound-ports": "http"}));
    let (init, _) = init_of(&p, &config);
    assert_eq!(env(&init, "MIMIC_INBOUND_PORTS"), Some("8080,9090"));
    assert!(!warnings(&p).is_empty());
    // `*` et `-` restent reconnus ; un pod sans annotation n'a aucun avertissement.
    for v in ["*", "-"] {
        let p = pod(json!({"mimic.platform.io/inbound-ports": v}));
        assert_eq!(env(&init_of(&p, &config).0, "MIMIC_INBOUND_PORTS"), Some(v));
        assert!(warnings(&p).is_empty());
    }
    assert!(warnings(&pod(json!({}))).is_empty());
}

#[test]
fn inj_19_injected_containers_follow_pod_security_hardening() {
    let config = config_with(YAML, None);
    let (init, p) = init_of(&pod(json!({})), &config);
    let sc = init.security_context.as_ref().unwrap();
    assert_eq!(sc.run_as_user, Some(0));
    assert_eq!(sc.allow_privilege_escalation, Some(false));
    assert_eq!(
        sc.seccomp_profile.as_ref().map(|s| s.type_.as_str()),
        Some("RuntimeDefault")
    );
    let caps = sc.capabilities.as_ref().unwrap();
    assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
    assert_eq!(
        caps.add.as_deref(),
        Some(&["NET_ADMIN".to_string(), "NET_RAW".to_string()][..])
    );
    let resources = init.resources.as_ref().expect("resources de mimic-init");
    assert!(resources.requests.as_ref().unwrap().contains_key("memory"));
    assert!(resources.limits.as_ref().unwrap().contains_key("memory"));

    let spec = p.spec.as_ref().unwrap();
    let sidecar = &spec.init_containers.as_ref().unwrap()[1];
    assert_eq!(
        sidecar
            .security_context
            .as_ref()
            .unwrap()
            .seccomp_profile
            .as_ref()
            .map(|s| s.type_.as_str()),
        Some("RuntimeDefault")
    );
    let ca = spec
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "mimic-controller-ca")
        .unwrap();
    assert_eq!(ca.config_map.as_ref().unwrap().optional, Some(true));

    // Ressources de mimic-init réglables.
    let yaml = format!("{YAML}init_resources:\n  limits: {{memory: 32Mi}}\n");
    let (init, _) = init_of(&pod(json!({})), &config_with(&yaml, None));
    assert_eq!(init.resources.unwrap().limits.unwrap()["memory"].0, "32Mi");
}

#[test]
fn inj_19_manual_example_marks_the_controller_ca_optional() {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../../../deploy/example/deployment.yaml")).unwrap();
    let volumes = doc["spec"]["template"]["spec"]["volumes"]
        .as_sequence()
        .unwrap();
    let ca = volumes
        .iter()
        .find(|v| v["name"] == "mimic-controller-ca")
        .unwrap();
    assert_eq!(ca["configMap"]["optional"], true);
    let init = &doc["spec"]["template"]["spec"]["initContainers"][0];
    assert_eq!(init["securityContext"]["allowPrivilegeEscalation"], false);
    assert_eq!(
        init["securityContext"]["seccompProfile"]["type"],
        "RuntimeDefault"
    );
    assert!(init["resources"]["limits"]["memory"].is_string());
}
