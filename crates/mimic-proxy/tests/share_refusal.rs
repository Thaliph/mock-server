//! Critère SHR-12 de la spec (§5.4, §9.1) : un `share.url` vers un stockage objet
//! (`s3://`, `file://`) est refusé au démarrage ; seul le contrôleur reçoit des échanges.

use mimic_proxy::config::Config;
use std::process::Command;

/// Chargement comme au démarrage : YAML en ligne (posé par le webhook) puis environnement.
fn load(yaml: &str) -> Result<Config, String> {
    let yaml = yaml.to_string();
    Config::load_with(None, move |k| match k {
        "MIMIC_CONFIG_YAML" => Some(yaml.clone()),
        "MIMIC_WORKLOAD" => Some("api".into()),
        "POD_NAMESPACE" => Some("ns".into()),
        "MIMIC_REDACT_SALT" => Some("sel".into()),
        _ => None,
    })
}

/// YAML d'une section `share` avec l'URL donnée, entre guillemets (espaces conservés).
fn share_yaml(url: &str) -> String {
    format!("share:\n  url: \"{url}\"\n")
}

/// Le message explicite : il nomme le champ et renvoie vers les deux solutions de remplacement.
fn assert_explicit_refusal(url: &str, err: &str) {
    assert!(err.contains("share.url"), "{url:?} : {err}");
    assert!(
        err.contains("contrôleur") && err.contains("https://"),
        "{url:?} : le message doit renvoyer vers l'URL du contrôleur : {err}"
    );
    assert!(
        err.contains("--mock-from"),
        "{url:?} : le message doit renvoyer vers --mock-from : {err}"
    );
    assert!(
        !err.contains("non supportée"),
        "{url:?} : message générique au lieu du message explicite : {err}"
    );
}

#[test]
fn shr_12_object_store_share_url_is_refused() {
    for url in [
        "s3://bucket/p",
        "file:///tmp/x",
        "S3://bucket",
        "FILE:///tmp/x",
        " s3://bucket",
        "\\ts3://bucket",
        "s3://bucket ",
    ] {
        let err = match load(&share_yaml(url)) {
            Ok(_) => panic!("{url:?} accepté : le partage direct doit être refusé"),
            Err(e) => e,
        };
        assert_explicit_refusal(url, &err);
    }
}

#[test]
fn shr_12_unknown_scheme_gets_the_same_explicit_message() {
    for url in ["ftp://x", "gs://bucket", "mimic-controller:8443", ""] {
        let err = match load(&share_yaml(url)) {
            Ok(_) => panic!("{url:?} accepté"),
            Err(e) => e,
        };
        assert_explicit_refusal(url, &err);
    }
}

#[test]
fn shr_12_refusal_does_not_depend_on_allow_insecure() {
    let yaml = "share:\n  url: s3://bucket\n  allow_insecure: true\n";
    let err = load(yaml).expect_err("s3:// accepté avec allow_insecure");
    assert_explicit_refusal("s3://bucket", &err);
}

#[test]
fn shr_12_controller_url_is_still_accepted() {
    let yaml =
        "share:\n  url: https://mimic-controller.mimic-system:8443\n  ca_file: /tmp/ca.crt\n";
    load(yaml).expect("l'URL du contrôleur reste valide");
    let insecure = "share:\n  url: http://127.0.0.1:8443\n  allow_insecure: true\n";
    load(insecure).expect("http:// avec allow_insecure reste valide (tests)");
}

#[test]
fn shr_12_retention_is_still_accepted() {
    // Champ conservé pour les configurations existantes : accepté, sans effet (WARN au démarrage).
    let yaml = "share:\n  url: https://mimic-controller.mimic-system:8443\n  ca_file: /tmp/ca.crt\n  retention: 3600\n";
    load(yaml).expect("share.retention reste accepté");
}

/// Lance le binaire avec une configuration en ligne ; `timeout` borne un démarrage fautif
/// (le processus resterait sinon à l'écoute) sans dépendre de la vitesse de la machine.
fn run_binary(yaml: &str) -> std::process::Output {
    Command::new("timeout")
        .arg("30")
        .arg(env!("CARGO_BIN_EXE_mimic-proxy"))
        .env_clear()
        .env("MIMIC_CONFIG_YAML", yaml)
        .env("MIMIC_WORKLOAD", "api")
        .env("MIMIC_REDACT_SALT", "sel")
        .output()
        .expect("lancement de mimic-proxy")
}

#[test]
fn shr_12_binary_exits_2_with_the_explicit_message() {
    for url in [
        "s3://bucket/p",
        "file:///tmp/x",
        "S3://bucket",
        " s3://bucket",
        "ftp://x",
    ] {
        let out = run_binary(&share_yaml(url));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{url:?} : {stderr}");
        assert_explicit_refusal(url, &stderr);
    }
}
