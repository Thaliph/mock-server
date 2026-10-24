//! Critères MON-01, MON-03, MON-04 et MON-05 de la spec (§9.3, §9.4), côté webhook.

use k8s_openapi::api::core::v1::{Container, Pod};
use mimic_injector::config::{InjectorConfig, TlsFiles};
use mimic_injector::inject::{Outcome, mutate, warnings};
use serde_json::json;

fn config(extra: &str) -> InjectorConfig {
    let config: InjectorConfig = serde_yaml::from_str(&format!(
        "image: ghcr.io/thaliph/mimic:0.1.0\nsidecar_config: \"mode: record\\n\"\n{extra}"
    ))
    .unwrap();
    config.validate().unwrap();
    config
}

fn pod(annotations: serde_json::Value) -> Pod {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "generateName": "checkout-7d9f-",
            "labels": {"app.kubernetes.io/name": "checkout"},
            "annotations": annotations
        },
        "spec": {"containers": [{"name": "app", "image": "checkout:1"}]}
    }))
    .unwrap()
}

fn sidecar(pod: &Pod, config: &InjectorConfig) -> (Pod, Container) {
    let Outcome::Injected(p) = mutate(pod, config) else {
        panic!("non injecté");
    };
    let side = p
        .spec
        .as_ref()
        .unwrap()
        .init_containers
        .iter()
        .flatten()
        .find(|c| c.name == "mimic")
        .unwrap()
        .clone();
    (*p, side)
}

fn env<'a>(c: &'a Container, name: &str) -> Option<&'a str> {
    c.env
        .iter()
        .flatten()
        .find(|e| e.name == name)
        .and_then(|e| e.value.as_deref())
}

#[test]
fn mon_03_sidecar_log_filter_from_annotation_then_webhook_config() {
    let plain = config("");
    let (_, side) = sidecar(&pod(json!({})), &plain);
    assert_eq!(env(&side, "MIMIC_LOG"), None, "sans réglage : info");

    let debug = config("sidecar_log: warn,mimic_proxy=debug\n");
    let (_, side) = sidecar(&pod(json!({})), &debug);
    assert_eq!(env(&side, "MIMIC_LOG"), Some("warn,mimic_proxy=debug"));

    let annotated = pod(json!({"mimic.platform.io/log": "debug"}));
    let (_, side) = sidecar(&annotated, &debug);
    assert_eq!(
        env(&side, "MIMIC_LOG"),
        Some("debug"),
        "l'annotation l'emporte"
    );
    assert!(warnings(&annotated).is_empty());
}

#[test]
fn mon_03_invalid_log_annotation_is_ignored_with_a_warning() {
    let debug = config("sidecar_log: debug\n");
    let invalid = pod(json!({"mimic.platform.io/log": "mimic_proxy=bavard"}));
    let (_, side) = sidecar(&invalid, &debug);
    assert_eq!(env(&side, "MIMIC_LOG"), Some("debug"));
    let w = warnings(&invalid);
    assert!(
        w.iter()
            .any(|w| w.contains("mimic.platform.io/log") && w.contains("mimic_proxy=bavard")),
        "{w:?}"
    );
}

#[test]
fn mon_03_invalid_sidecar_log_is_refused_at_startup() {
    let config: InjectorConfig = serde_yaml::from_str(
        "image: mimic:test\nsidecar_config: \"mode: record\\n\"\nsidecar_log: \"mimic_proxy=bavard\"\n",
    )
    .unwrap();
    assert!(config.validate().unwrap_err().contains("sidecar_log"));
}

/// Quantité Kubernetes en octets (`512Mi`, `1Gi`, nombre d'octets).
fn bytes(q: &str) -> u64 {
    for (suffix, unit) in [("Ki", 1u64 << 10), ("Mi", 1 << 20), ("Gi", 1 << 30)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.parse::<u64>().unwrap() * unit;
        }
    }
    q.parse().unwrap()
}

#[test]
fn mon_05_sidecar_knows_the_size_of_its_store() {
    let (p, side) = sidecar(&pod(json!({})), &config(""));
    let store = p
        .spec
        .as_ref()
        .unwrap()
        .volumes
        .iter()
        .flatten()
        .find(|v| v.name == "mimic-store")
        .unwrap();
    let limit = &store
        .empty_dir
        .as_ref()
        .unwrap()
        .size_limit
        .as_ref()
        .unwrap()
        .0;
    assert_eq!(
        env(&side, "MIMIC_STORE_SIZE_LIMIT_BYTES"),
        Some(bytes(limit).to_string().as_str())
    );
}

/// Certificat auto-signé pour `localhost` qui expire le 2 janvier 2031.
fn expiring_2031() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.not_after = rcgen::date_time_ymd(2031, 1, 2);
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

#[tokio::test]
async fn mon_01_mon_04_webhook_exposes_build_info_and_certificate_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = expiring_2031();
    let files = TlsFiles::new(dir.path().join("tls.crt"), dir.path().join("tls.key"));
    std::fs::write(&files.cert_file, &cert).unwrap();
    std::fs::write(&files.key_file, key).unwrap();
    let mut c = config("listen: 127.0.0.1:0\n");
    c.tls = Some(files);
    let running = mimic_injector::server::start(c).await.unwrap();
    let m = reqwest::Client::builder()
        .no_proxy()
        .tls_certs_only([reqwest::Certificate::from_pem(cert.as_bytes()).unwrap()])
        .build()
        .unwrap()
        .get(format!("https://localhost:{}/metrics", running.addr.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        m.lines()
            .any(|l| l == "mimic_injector_certificate_expiry_timestamp_seconds 1925078400"),
        "{m}"
    );
    let prefix = format!(
        "mimic_build_info{{mimic_version=\"{}\",revision=\"",
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        m.lines()
            .any(|l| l.starts_with(&prefix) && l.ends_with("\"} 1")),
        "{m}"
    );
    running.shutdown();
}
