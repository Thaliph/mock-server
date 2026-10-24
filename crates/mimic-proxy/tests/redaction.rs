//! Critères RED-06, RED-08 et RED-10 de la spec (§7) : sel obligatoire, requête du mock
//! redactée avant le matching, bodies non analysables.

mod common;

use common::*;
use mimic_proxy::config::{Config, Mode, ShareConfig};
use serde_json::{Value, json};

#[test]
fn red_06_salt_is_mandatory_without_controller() {
    let mut alone = config(None, Mode::Record);
    alone.redact.salt = None;
    let err = alone.validate().unwrap_err();
    assert!(
        err.contains("redact.salt") && err.contains("MIMIC_REDACT_SALT"),
        "{err}"
    );

    // Partage direct (sans contrôleur) : pas de sel distribué non plus.
    let mut direct = alone.clone();
    direct.share = Some(ShareConfig {
        url: "file:///tmp/partage".into(),
        ..ShareConfig::default()
    });
    assert!(direct.validate().is_err());

    // Sous contrôleur : le sel du workload viendra du battement.
    let mut piloted = alone.clone();
    piloted.share = Some(ShareConfig {
        url: "http://127.0.0.1:1".into(),
        allow_insecure: true,
        ..ShareConfig::default()
    });
    piloted.validate().unwrap();

    let mut salted = alone;
    salted.redact.salt = Some("sel".into());
    salted.validate().unwrap();
}

#[tokio::test]
async fn red_06_sidecar_without_salt_refuses_to_start() {
    let mut c = config(None, Mode::Record);
    c.redact.salt = None;
    let err = match mimic_proxy::start(c).await {
        Ok(_) => panic!("démarré sans sel"),
        Err(e) => e,
    };
    assert!(err.contains("MIMIC_REDACT_SALT"), "{err}");
}

/// En-tête `x-mimic-match` d'une réponse, vide s'il manque.
fn matched(r: &reqwest::Response) -> String {
    r.headers()
        .get("x-mimic-match")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

async fn post(p: &mimic_proxy::Running, url: &str, key: &str, email: &str) -> reqwest::Response {
    app_client(p)
        .post(url)
        .header("x-api-key", key)
        .json(&json!({"email": email, "n": 1}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn red_08_mock_matches_the_redacted_request() {
    let saas = fake_server().await;
    let url = saas.url("/echo");
    let mut c = config(None, Mode::Record);
    c.mock.match_headers = vec!["x-api-key".into()];
    let p = proxy(c).await;
    assert_eq!(post(&p, &url, "k1", "a@b.io").await.status(), 200);
    wait_exchanges(&p, "direction=outbound", 1).await;

    set_mode(&p, "mock").await;
    let hits = saas.hits();
    let r = post(&p, &url, "k1", "a@b.io").await;
    assert!(matched(&r).starts_with("exact"), "{:?}", r.headers());
    let r = post(&p, &url, "k2", "a@b.io").await;
    assert!(!matched(&r).starts_with("exact"), "{:?}", r.headers());
    let r = post(&p, &url, "k1", "c@d.io").await;
    assert!(!matched(&r).starts_with("exact"), "{:?}", r.headers());
    assert_eq!(saas.hits(), hits, "rien n'est sorti en mock");

    // mock-partial : une requête sans enregistrement (autre dépendance) est relayée intacte.
    set_mode(&p, "mock-partial").await;
    let other = fake_server().await;
    let r = post(&p, &other.url("/echo"), "k9", "e@f.io").await;
    assert_eq!(r.status(), 200);
    let echoed: Value = r.json().await.unwrap();
    assert_eq!(echoed["email"], "e@f.io");
}

#[tokio::test]
async fn red_10_unredactable_bodies_are_not_captured_by_default() {
    let saas = fake_server().await;
    let url = saas.url("/echo");
    let send = |p: &mimic_proxy::Running| {
        app_client(p)
            .post(url.clone())
            .header("content-type", "application/xml")
            .body("<a><token>t</token></a>")
            .send()
    };

    let p = proxy(config(None, Mode::Record)).await;
    assert_eq!(send(&p).await.unwrap().status(), 200);
    let list = wait_exchanges(&p, "direction=outbound", 1).await;
    for side in ["req_body", "resp_body"] {
        assert_eq!(
            list[0][side],
            json!({"kind": "skipped", "size": 23}),
            "{side}"
        );
    }
    let m = metrics(&p).await;
    assert_eq!(
        metric_value(&m, r#"mimic_capture_dropped_total{reason="unredactable"}"#),
        Some(2.0),
        "{m}"
    );

    // Exception par hôte : gardés tels quels.
    let mut c = config(None, Mode::Record);
    c.redact.raw_bodies = vec!["127.0.0.1".into()];
    let p = proxy(c).await;
    assert_eq!(send(&p).await.unwrap().status(), 200);
    let list = wait_exchanges(&p, "direction=outbound", 1).await;
    assert_eq!(list[0]["resp_body"]["kind"], "captured", "{}", list[0]);
}

#[test]
fn red_10_raw_bodies_come_from_the_environment() {
    let mut c: Config =
        serde_yaml::from_str("redact: { salt: s, raw_bodies: [api.partner.io] }").unwrap();
    assert_eq!(c.redact.raw_bodies, ["api.partner.io"]);
    c.apply_env(|k| {
        (k == "MIMIC_REDACT_RAW_BODIES").then(|| " *.partner.io , inbound ,".to_string())
    })
    .unwrap();
    assert_eq!(c.redact.raw_bodies, ["*.partner.io", "inbound"]);
}
