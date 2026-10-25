//! Webhook servi en HTTPS, de la revue d'admission au pod patché (spec §9.4, INJ-08), avec
//! un certificat relu à chaud (INJ-14) et un arrêt propre sur `SIGTERM` (INJ-16).

use std::time::{Duration, Instant};

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

/// Certificat auto-signé pour `localhost` : (certificat PEM, clé PEM).
fn self_signed() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (cert.pem(), key.serialize_pem())
}

/// Client qui ne fait confiance qu'à `cert`.
fn client_trusting(cert: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .tls_certs_only([reqwest::Certificate::from_pem(cert.as_bytes()).unwrap()])
        .build()
        .unwrap()
}

async fn webhook() -> Webhook {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = self_signed();
    let cert_file = dir.path().join("tls.crt");
    let key_file = dir.path().join("tls.key");
    std::fs::write(&cert_file, &cert).unwrap();
    std::fs::write(&key_file, key).unwrap();
    let mut config: InjectorConfig = serde_yaml::from_str(
        "listen: 127.0.0.1:0\nimage: mimic:test\ninit_image: mimic-init:test\nsidecar_config: \"mode: record\\n\"\n",
    )
    .unwrap();
    let mut tls = TlsFiles::new(cert_file, key_file);
    tls.reload_interval = Duration::from_millis(100);
    config.tls = Some(tls);
    config.validate().unwrap();
    let running = mimic_injector::server::start(config.clone()).await.unwrap();
    let client = client_trusting(&cert);
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
        Outcome::Unchanged => panic!("déjà injecté"),
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
    let config: InjectorConfig = serde_yaml::from_str(
        "image: mimic:test\ninit_image: mimic-init:test\nsidecar_config: \"mood: record\\n\"\n",
    )
    .unwrap();
    assert!(config.validate().unwrap_err().contains("sidecar_config"));
}

#[tokio::test]
async fn inj_14_renewed_certificate_is_served_without_restart() {
    let w = webhook().await;
    let healthz = format!("{}/healthz", w.url);
    assert!(w.client.get(&healthz).send().await.is_ok());
    let tls = w.config.tls.clone().unwrap();
    let (cert, key) = self_signed();
    std::fs::write(&tls.cert_file, &cert).unwrap();
    std::fs::write(&tls.key_file, key).unwrap();
    let renewed = client_trusting(&cert);
    let deadline = Instant::now() + Duration::from_secs(5);
    while renewed.get(&healthz).send().await.is_err() {
        assert!(
            Instant::now() < deadline,
            "certificat renouvelé jamais servi"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Contenu illisible : le certificat renouvelé reste servi.
    std::fs::write(&tls.cert_file, "illisible").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(renewed.get(&healthz).send().await.is_ok());
    w.running.shutdown();
}

#[tokio::test]
async fn inj_14_unreadable_certificate_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tls.crt"), "x").unwrap();
    std::fs::write(dir.path().join("tls.key"), "y").unwrap();
    let mut config: InjectorConfig =
        serde_yaml::from_str("listen: 127.0.0.1:0\nimage: mimic:test\ninit_image: mimic-init:test\nsidecar_config: \"{}\"\n")
            .unwrap();
    config.tls = Some(TlsFiles::new(
        dir.path().join("tls.crt"),
        dir.path().join("tls.key"),
    ));
    assert!(mimic_injector::server::start(config).await.is_err());
}

#[test]
fn inj_16_sigterm_stops_the_webhook_cleanly() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("injector.yaml");
    std::fs::write(
        &path,
        "listen: 127.0.0.1:0\nimage: mimic:test\ninit_image: mimic-init:test\nsidecar_config: \"mode: record\\n\"\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mimic-injector"))
        .arg("--config")
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Les logs sont lus jusqu'au bout : le journal de l'arrêt ne trouve pas un tube fermé.
    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            if line.contains("webhook d'injection démarré") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("webhook jamais démarré");
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("webhook toujours là 5 s après SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}");
}
