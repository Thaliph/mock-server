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

/// Labels nommés `mimic_…`, cités à côté des métriques (`mimic_build_info`).
const LABELS: [&str; 1] = ["mimic_version"];

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
                known.contains(&base)
                    || known.contains(&cited.as_str())
                    || crates.contains(&base)
                    || LABELS.contains(&base),
                "{cited} : citée par le guide, enregistrée par personne"
            );
        }
    }
}

/// Les règles d'alerte livrées ne citent que des métriques enregistrées (MON-06).
#[test]
fn mon_06_rules_cite_only_registered_metrics() {
    let mut known = registered(SIDECAR_METRICS);
    known.extend(registered(CONTROLLER_METRICS));
    known.extend(registered(INJECTOR_METRICS));
    let cited = cited_metrics(include_str!("../../../deploy/monitoring/rules.yaml"));
    assert!(cited.len() > 20, "{cited:?}");
    for name in cited {
        let base = ["_total", "_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| name.strip_suffix(s))
            .unwrap_or(name.as_str());
        assert!(
            known.contains(&base) || known.contains(&name.as_str()) || LABELS.contains(&base),
            "{name} : citée par les règles, enregistrée par personne"
        );
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

const PROXY: &str = include_str!("../src/proxy.rs");
const DEV_DOC: &str = include_str!("../../../docs/guide/Developpeur-applicatif.md");

/// Code HTTP d'un nom de `StatusCode` employé par le sidecar.
fn code(name: &str) -> u16 {
    match name {
        "BAD_REQUEST" => 400,
        "FORBIDDEN" => 403,
        "PAYLOAD_TOO_LARGE" => 413,
        "BAD_GATEWAY" => 502,
        "SERVICE_UNAVAILABLE" => 503,
        "GATEWAY_TIMEOUT" => 504,
        other => panic!("StatusCode::{other} non répertorié par DOC-08"),
    }
}

/// Réponses d'erreur du sidecar : (code, corps `mimic: …`), lues dans le source.
fn error_responses() -> Vec<(u16, String)> {
    let mut out = vec![];
    let mut rest = PROXY;
    while let Some(at) = rest.find("\"mimic: ") {
        let mut start = at.saturating_sub(160);
        while !rest.is_char_boundary(start) {
            start -= 1;
        }
        let before = &rest[start..at];
        let body_start = &rest[at + 1..];
        let body = &body_start[..body_start.find('"').unwrap()];
        let code = if before.contains("from_u16(599)") {
            599
        } else {
            let name = before.rsplit("StatusCode::").next().unwrap();
            let name: String = name
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                .collect();
            code(&name)
        };
        out.push((code, body.to_string()));
        rest = &rest[at + 1 + body.len()..];
    }
    out
}

/// La page des équipes applicatives existe, et décrit tout ce que le sidecar leur renvoie :
/// en-têtes `x-mimic-*` et réponses d'erreur, avec leur code (DOC-08).
#[test]
fn doc_08_application_developer_page() {
    let readme = include_str!("../../../docs/guide/README.md");
    assert!(readme.contains("(Developpeur-applicatif.md)"), "sommaire");
    for section in ["## Glossaire", "## Ce que la plateforme doit fournir"] {
        assert!(DEV_DOC.contains(section), "{section} absente");
    }
    let rbac = include_str!("../../../deploy/controller/rbac.yaml");
    assert!(rbac.contains("name: mimic-data-viewer"));
    assert!(DEV_DOC.contains("`mimic-data-viewer`"));

    let headers: Vec<&str> = PROXY
        .split('"')
        .filter(|t| t.starts_with("x-mimic-") && !t.contains(' '))
        .collect();
    assert!(headers.len() >= 3, "{headers:?}");
    for h in headers {
        assert!(DEV_DOC.contains(&format!("`{h}`")), "{h} absent de la page");
    }

    let errors = error_responses();
    assert!(errors.len() >= 9, "{errors:?}");
    for (code, body) in errors {
        assert!(
            DEV_DOC
                .lines()
                .any(|l| l.contains(&format!("`{code}`")) && l.contains(&body)),
            "« {body} » ({code}) absent de la page, ou sans son code sur la même ligne"
        );
    }
}
