//! Annotations du pod reprises dans les règles de `mimic-init` (`inject::mutate`, qui les
//! analyse avec `checked`) : la valeur posée est canonique et relue à l'identique par
//! `mimic-init` ou par le plugin CNI ; une valeur invalide n'est jamais recopiée.
//!
//! Entrée, lignes séparées par `\n` : drapeaux (`c` variante CNI, `i` interception
//! entrante), puis les valeurs de `inbound-ports`, `exclude-outbound-cidrs`,
//! `exclude-outbound-ports` et `log` ; une ligne vide ou absente laisse l'annotation absente.
#![no_main]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use k8s_openapi::api::core::v1::{Container, ContainerPort, Pod, PodSpec};
use libfuzzer_sys::fuzz_target;
use mimic_init::InboundPorts;
use mimic_injector::config::InjectorConfig;
use mimic_injector::inject::{
    ANNOTATION_CNI_RULES, ANNOTATION_EXCLUDE_CIDRS, ANNOTATION_EXCLUDE_PORTS,
    ANNOTATION_INBOUND_PORTS, ANNOTATION_LOG, Outcome, mutate, valid_log_filter,
};

const CONFIG: &str = r#"
image: ghcr.io/thaliph/mimic:0.1.0
init_image: ghcr.io/thaliph/mimic-init:0.1.0
exclude_outbound_cidrs: [10.96.0.1/32]
sidecar_config: |
  mode: record
  store: { dir: /var/lib/mimic }
"#;

static CONFIGS: LazyLock<[InjectorConfig; 4]> = LazyLock::new(|| {
    let base: InjectorConfig = serde_yaml::from_str(CONFIG).unwrap();
    [(false, false), (false, true), (true, false), (true, true)].map(|(cni, inbound)| {
        let mut c = base.clone();
        c.cni = cni;
        c.inbound = inbound;
        c
    })
});

fn pod(annotations: BTreeMap<String, String>) -> Pod {
    Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("checkout-0".into()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                "checkout".into(),
            )])),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "app".into(),
                image: Some("registry.example.com/checkout:1".into()),
                ports: Some(vec![ContainerPort {
                    container_port: 8080,
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Variables de `mimic-init` du pod injecté : conteneur `mimic-init`, ou annotation relue
/// par le plugin CNI.
fn rules(pod: &Pod) -> BTreeMap<String, String> {
    if let Some(rules) = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(ANNOTATION_CNI_RULES))
    {
        // Le plugin refuse toute variable inconnue : il doit accepter ce que pose le webhook.
        mimic_cni::settings(rules, None)
            .unwrap_or_else(|e| panic!("annotation CNI refusée : {e} ({rules})"));
        return serde_json::from_str(rules).unwrap();
    }
    container(pod, "mimic-init")
        .env
        .iter()
        .flatten()
        .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
        .collect()
}

fn container<'a>(pod: &'a Pod, name: &str) -> &'a Container {
    pod.spec
        .iter()
        .flat_map(|s| s.init_containers.iter().flatten())
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("conteneur {name} absent"))
}

fn join<T: ToString>(items: &[T]) -> String {
    items.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut lines = text.split('\n');
    let flags = lines.next().unwrap_or_default();
    let mut annotations = BTreeMap::new();
    for key in [
        ANNOTATION_INBOUND_PORTS,
        ANNOTATION_EXCLUDE_CIDRS,
        ANNOTATION_EXCLUDE_PORTS,
        ANNOTATION_LOG,
    ] {
        match lines.next() {
            Some(v) if !v.is_empty() => {
                annotations.insert(key.to_string(), v.to_string());
            }
            _ => {}
        }
    }
    let cfg = &CONFIGS[usize::from(flags.contains('c')) * 2 + usize::from(flags.contains('i'))];
    let input = pod(annotations);
    let out = match mutate(&input, cfg) {
        Outcome::Injected(p) => *p,
        other => panic!("pod non injecté : {other:?}"),
    };

    let env = rules(&out);
    let inbound = env.get("MIMIC_INBOUND_PORTS").expect("MIMIC_INBOUND_PORTS");
    match InboundPorts::parse(inbound) {
        Ok(InboundPorts::List(ports)) => assert_eq!(&join(&ports), inbound, "non canonique"),
        Ok(_) => assert!(
            inbound == "*" || inbound == "-",
            "non canonique : {inbound:?}"
        ),
        Err(e) => panic!("MIMIC_INBOUND_PORTS refusé par mimic-init : {e}"),
    }
    let cidrs = env.get("MIMIC_EXCLUDE_OUTBOUND_CIDRS").expect("CIDR");
    let parsed = mimic_init::parse_cidrs(cidrs)
        .unwrap_or_else(|e| panic!("MIMIC_EXCLUDE_OUTBOUND_CIDRS refusé : {e}"));
    assert_eq!(&join(&parsed), cidrs, "CIDR non canoniques");
    if let Some(ports) = env.get("MIMIC_EXCLUDE_OUTBOUND_PORTS") {
        let parsed = mimic_init::parse_ports(ports)
            .unwrap_or_else(|e| panic!("MIMIC_EXCLUDE_OUTBOUND_PORTS refusé : {e}"));
        assert_eq!(&join(&parsed), ports, "ports non canoniques");
    }
    // Rien d'autre qu'une valeur canonique : pas d'espace, donc pas d'option iptables glissée.
    for (k, v) in &env {
        if k != "MIMIC_MESH" {
            assert!(!v.contains(char::is_whitespace), "{k} = {v:?}");
        }
    }

    // Filtre de logs du sidecar : valide, sinon absent.
    let sidecar = container(&out, "mimic");
    if let Some(log) = sidecar
        .env
        .iter()
        .flatten()
        .find(|e| e.name == "MIMIC_LOG")
        .and_then(|e| e.value.as_deref())
    {
        valid_log_filter(log).unwrap_or_else(|e| panic!("MIMIC_LOG invalide {log:?} : {e}"));
    }

    // Déterministe, et idempotent à la réinvocation.
    match mutate(&input, cfg) {
        Outcome::Injected(p) => assert_eq!(*p, out, "mutation non déterministe"),
        other => panic!("seconde mutation : {other:?}"),
    }
    assert!(matches!(mutate(&out, cfg), Outcome::Unchanged));
});
