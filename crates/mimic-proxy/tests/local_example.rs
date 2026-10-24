//! Critère DOC-09 de la spec (§9.7) : l'exemple docker-compose du mock local.

use mimic_proxy::config::Config;

const COMPOSE: &str = include_str!("../../../deploy/local/docker-compose.yaml");
const CONFIG: &str = include_str!("../../../deploy/local/mimic.yaml");
const SNAPSHOT: &str = include_str!("../../../deploy/local/snapshot.jsonl");

#[test]
fn doc_09_local_example_is_consistent() {
    let compose: serde_yaml::Value = serde_yaml::from_str(COMPOSE).unwrap();
    let mimic = &compose["services"]["mimic"];
    let command: Vec<&str> = mimic["command"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let at = command
        .iter()
        .position(|a| *a == "--mock-from")
        .expect("--mock-from");
    let file = command[at + 1];
    assert!(file.ends_with("snapshot.jsonl"), "{file}");
    assert!(
        mimic["volumes"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().starts_with("./")),
        "fichiers de l'exemple montés"
    );

    let mut config: Config = serde_yaml::from_str(CONFIG).unwrap();
    config.mock.from = Some(file.into());
    config.validate().unwrap();
    assert!(!config.outbound.listen.ip().is_loopback());

    let app = &compose["services"]["app"];
    let env = app["environment"].as_mapping().unwrap();
    let proxy = env
        .get(serde_yaml::Value::from("HTTP_PROXY"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(
        proxy,
        format!("http://mimic:{}", config.outbound.listen.port())
    );
    let url = env
        .get(serde_yaml::Value::from("DEPENDENCY_URL"))
        .and_then(|v| v.as_str())
        .unwrap();

    let snap = mimic_core::snapshot::Snapshot::parse(SNAPSHOT.as_bytes()).unwrap();
    let e = &snap.exchanges[0];
    let query: Vec<String> = e.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let recorded = format!(
        "{}://{}{}?{}",
        e.scheme,
        e.authority,
        e.path,
        query.join("&")
    );
    assert_eq!(url, recorded);
}
