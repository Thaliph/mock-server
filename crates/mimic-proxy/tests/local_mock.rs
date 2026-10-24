//! Critère PRX-26 de la spec (§9.7) : mock local servi depuis un fichier, sans contrôleur.

mod common;

use common::*;
use mimic_proxy::config::{Config, Mode, ShareConfig};
use serde_json::{Value, json};

/// Snapshot JSONL d'une dépendance qui n'existe pas.
fn snapshot_line() -> String {
    let e = json!({
        "id": "local-1", "direction": "outbound", "started_at_ms": 1, "scheme": "http",
        "authority": "quotes.internal:8080", "method": "GET", "path": "/quote",
        "query": [["zip", "75001"]], "req_headers": [], "req_body": {"kind": "empty"},
        "status": 200,
        "resp_headers": [{"name": "content-type", "value": "application/json"}],
        "resp_body": {"kind": "captured", "data": "eyJwcmljZSI6NDJ9"},
        "ttfb_us": 1000, "total_us": 2000, "trace_id": null
    });
    format!("{e}\n")
}

fn local(file: &std::path::Path) -> Config {
    let mut c = config(None, Mode::Record);
    c.redact.salt = None;
    c.mock.from = Some(file.to_path_buf());
    c
}

#[tokio::test]
async fn prx_26_snapshot_file_is_served_without_controller() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snapshot.jsonl");
    std::fs::write(&file, snapshot_line()).unwrap();
    let p = proxy(local(&file)).await;

    let status: Value = client()
        .get(admin(&p, "/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["mode"], "mock", "{status}");
    assert!(status["snapshot"].is_string(), "{status}");

    let app = app_client(&p);
    let r = app
        .get("http://quotes.internal:8080/quote?zip=75001")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mimic-match"));
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"price": 42}));
    let r = app
        .get("http://quotes.internal:8080/other")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 599);
}

#[test]
fn prx_26_mock_from_is_local_only_and_comes_from_the_environment() {
    let file = std::path::PathBuf::from("/tmp/snapshot.jsonl");
    let mut c = local(&file);
    c.validate().unwrap();
    c.share = Some(ShareConfig {
        url: "http://127.0.0.1:1".into(),
        allow_insecure: true,
        ..ShareConfig::default()
    });
    let err = c.validate().unwrap_err();
    assert!(err.contains("mock.from"), "{err}");

    let mut c: Config = serde_yaml::from_str("redact: { salt: s }").unwrap();
    c.apply_env(|k| (k == "MIMIC_MOCK_FROM").then(|| "/data/s.jsonl".into()))
        .unwrap();
    assert_eq!(
        c.mock.from.as_deref(),
        Some(std::path::Path::new("/data/s.jsonl"))
    );
    assert_eq!(c.mode, Mode::Mock, "le mock local démarre en mock");
}

#[tokio::test]
async fn prx_26_unreadable_file_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("abime.jsonl");
    std::fs::write(&file, "pas du json\n").unwrap();
    let err = match mimic_proxy::start(local(&file)).await {
        Ok(_) => panic!("démarré"),
        Err(e) => e,
    };
    assert!(err.contains("abime.jsonl"), "{err}");
}
