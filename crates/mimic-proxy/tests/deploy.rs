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

/// Noms des métriques enregistrées par un module de métriques : chaque appel à
/// `registry.register` passe son nom en littéral, un littéral `mimic_…` par métrique.
/// Prometheus expose un compteur `mimic_x` sous `mimic_x_total` ; une jauge et un
/// histogramme gardent leur nom.
fn registered(source: &str) -> Vec<&str> {
    let names: Vec<&str> = source
        .split('"')
        .filter(|t| t.starts_with("mimic_") && !t.contains(' '))
        .collect();
    assert_eq!(
        names.len(),
        source.matches("registry.register").count(),
        "un littéral mimic_… par enregistrement attendu : {names:?}"
    );
    names
}

const SIDECAR_METRICS: &str = include_str!("../src/metrics.rs");
const CONTROLLER_METRICS: &str = include_str!("../../mimic-controller/src/metrics.rs");
const INJECTOR_METRICS: &str = include_str!("../../mimic-injector/src/server.rs");
const METRICS_DOC: &str = include_str!("../../../docs/guide/Metriques.md");

/// Pages du guide qui citent des métriques.
const GUIDE: [&str; 5] = [
    METRICS_DOC,
    include_str!("../../../docs/guide/Alertes-et-supervision.md"),
    include_str!("../../../docs/guide/Exploiter-en-production.md"),
    include_str!("../../../docs/guide/Tir-de-charge.md"),
    include_str!("../../../docs/guide/Depannage.md"),
];

/// Le guide documente cette métrique, sous son nom ou sous son nom exposé.
fn documented(name: &str) -> bool {
    METRICS_DOC.contains(&format!("`{name}`")) || METRICS_DOC.contains(&format!("`{name}_total`"))
}

/// Toute métrique enregistrée par le sidecar est documentée dans le guide (DOC-01).
#[test]
fn doc_01_sidecar_metrics_are_documented() {
    let names = registered(SIDECAR_METRICS);
    assert!(names.len() > 15, "registre trop court : {names:?}");
    for name in names {
        assert!(
            documented(name),
            "métrique {name} absente de docs/guide/Metriques.md"
        );
    }
}

/// Les métriques du contrôleur et du webhook sont documentées, et le guide n'en invente
/// aucune : tout nom `mimic_…` qu'il cite est une métrique enregistrée ou un nom de crate
/// (DOC-02).
#[test]
fn doc_02_control_plane_metrics_are_documented_both_ways() {
    let control_plane: Vec<&str> = registered(CONTROLLER_METRICS)
        .into_iter()
        .chain(registered(INJECTOR_METRICS))
        .collect();
    for name in &control_plane {
        assert!(
            documented(name),
            "métrique {name} absente de docs/guide/Metriques.md"
        );
    }
    let mut known = registered(SIDECAR_METRICS);
    known.extend(control_plane);

    // Sens inverse : aucune métrique inventée dans un tableau ou une requête PromQL.
    let crates = [
        "mimic_core",
        "mimic_net",
        "mimic_init",
        "mimic_proxy",
        "mimic_controller",
        "mimic_injector",
    ];
    for page in GUIDE {
        for cited in cited_metrics(page) {
            let base = ["_total", "_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| cited.strip_suffix(s))
                .unwrap_or(cited.as_str());
            assert!(
                known.contains(&base) || known.contains(&cited.as_str()) || crates.contains(&base),
                "{cited} : citée par le guide, enregistrée par personne"
            );
        }
    }
}

/// Noms `mimic_…` cités par une page du guide, hors mots composés (`MIMIC_LOG`).
fn cited_metrics(page: &str) -> Vec<String> {
    let bytes = page.as_bytes();
    let word = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(start) = page[i..].find("mimic_").map(|p| p + i) {
        let mut end = start;
        while end < bytes.len() && word(bytes[end]) {
            end += 1;
        }
        let joined =
            start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        if !joined {
            out.push(page[start..end].to_string());
        }
        i = end.max(start + 1);
    }
    out.sort();
    out.dedup();
    out
}
