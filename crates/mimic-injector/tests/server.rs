//! Webhook servi en HTTPS, de la revue d'admission au pod patché (spec §9.4, INJ-08).

use mimic_injector::config::{InjectorConfig, TlsFiles};
use mimic_injector::inject::{Outcome, mutate};
use serde_json::{Value, json};

fn pod() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "checkout-1", "namespace": "shop",
                     "labels": {"app.kubernetes.io/name": "checkout"}},
        "spec": {"containers": [{"name": "app", "image": "app:1", "ports": [{"containerPort": 8080}]}]}
    })
}

fn review(object: Value) -> Value {
    json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "705ab4f5-6393-11e8-b7cc-42010a800002",
            "kind": {"group": "", "version": "v1", "kind": "Pod"},
            "resource": {"group": "", "version": "v1", "resource": "pods"},
            "requestKind": {"group": "", "version": "v1", "kind": "Pod"},
            "requestResource": {"group": "", "version": "v1", "resource": "pods"},
            "namespace": "shop",
            "operation": "CREATE",
            "userInfo": {"username": "system:serviceaccount:kube-system:replicaset-controller"},
            "object": object,
            "dryRun": false
        }
    })
}

struct Webhook {
    url: String,
    client: reqwest::Client,
    config: InjectorConfig,
    _dir: tempfile::TempDir,
    running: mimic_injector::server::Running,
}

async fn webhook() -> Webhook {
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_file = dir.path().join("tls.crt");
    let key_file = dir.path().join("tls.key");
    std::fs::write(&cert_file, cert.pem()).unwrap();
    std::fs::write(&key_file, key.serialize_pem()).unwrap();
    let mut config: InjectorConfig = serde_yaml::from_str(
        "listen: 127.0.0.1:0\nimage: mimic:test\nsidecar_config: \"mode: record\\n\"\n",
    )
    .unwrap();
    config.tls = Some(TlsFiles {
        cert_file,
        key_file,
    });
    config.validate().unwrap();
    let running = mimic_injector::server::start(config.clone()).await.unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_certs_only([reqwest::Certificate::from_pem(cert.pem().as_bytes()).unwrap()])
        .build()
        .unwrap();
    Webhook {
        url: format!("https://localhost:{}", running.addr.port()),
        client,
        config,
        _dir: dir,
        running,
    }
}

#[tokio::test]
async fn inj_08_admission_review_returns_a_patch_that_injects_the_pod() {
    let w = webhook().await;
    let resp: Value = w
        .client
        .post(format!("{}/mutate", w.url))
        .json(&review(pod()))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = &resp["response"];
    assert_eq!(resp["kind"], "AdmissionReview");
    assert_eq!(r["uid"], "705ab4f5-6393-11e8-b7cc-42010a800002");
    assert_eq!(r["allowed"], true);
    assert_eq!(r["patchType"], "JSONPatch");

    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(r["patch"].as_str().unwrap())
        .unwrap();
    let patch: json_patch::Patch = serde_json::from_slice(&raw).unwrap();
    let mut patched = pod();
    json_patch::patch(&mut patched, &patch).unwrap();

    let expected = match mutate(&serde_json::from_value(pod()).unwrap(), &w.config) {
        Outcome::Injected(p) => serde_json::to_value(*p).unwrap(),
        Outcome::Skipped(r) => panic!("{r}"),
    };
    assert_eq!(patched, expected);
    assert_eq!(patched["spec"]["initContainers"][1]["name"], "mimic");

    let metrics = w
        .client
        .get(format!("{}/metrics", w.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("mimic_injector_admissions_total{result=\"injected\"} 1"),
        "{metrics}"
    );
    w.running.shutdown();
}

#[tokio::test]
async fn inj_08_skipped_pod_is_allowed_with_a_warning() {
    let w = webhook().await;
    let mut p = pod();
    p["metadata"]["annotations"] = json!({"mimic.platform.io/inject": "false"});
    let resp: Value = w
        .client
        .post(format!("{}/mutate", w.url))
        .json(&review(p))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = &resp["response"];
    assert_eq!(r["allowed"], true);
    assert!(r.get("patch").is_none_or(Value::is_null));
    assert!(r["warnings"][0].as_str().unwrap().contains("non injecté"));
    w.running.shutdown();
}

#[test]
fn invalid_sidecar_config_is_refused_at_startup() {
    let config: InjectorConfig =
        serde_yaml::from_str("image: mimic:test\nsidecar_config: \"mood: record\\n\"\n").unwrap();
    assert!(config.validate().unwrap_err().contains("sidecar_config"));
}
