//! Critères MON-01, MON-02 et MON-04 de la spec (§9.2b, §9.3), côté contrôleur.

use std::collections::BTreeMap;
use std::time::Duration;

use mimic_controller::config::{AuthConfig, ControllerConfig, Identity, TlsFiles};
use mimic_controller::crd::CaptureSpec;
use mimic_controller::reconcile::MimicWorkloadSpec;
use mimic_controller::{Running, start};
use serde_json::json;

fn config(dir: &std::path::Path) -> ControllerConfig {
    let tokens: BTreeMap<String, Identity> = [("checkout-a", "checkout"), ("web-a", "web")]
        .iter()
        .map(|(pod, wl)| {
            (
                format!("tok-{pod}"),
                Identity {
                    namespace: "shop".into(),
                    pod: pod.to_string(),
                    workload: wl.to_string(),
                    ..Identity::default()
                },
            )
        })
        .collect();
    ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ui_listen: "127.0.0.1:0".parse().unwrap(),
        store: format!("file://{}", dir.display()),
        auth: AuthConfig::Static { tokens },
        ..ControllerConfig::default()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn metrics(c: &Running) -> String {
    client()
        .get(format!("http://{}/metrics", c.ui_addr()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// Battement d'un pod en `record`, sans capture entrante annoncée.
async fn heartbeat(c: &Running, scheme: &str, instance: &str) {
    let workload = instance.rsplit_once('-').unwrap().0;
    let r = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .post(format!(
            "{scheme}://{}/v1/workloads/shop/{workload}/heartbeat",
            c.api_addr()
        ))
        .bearer_auth(format!("tok-{instance}"))
        .json(&json!({"instance": instance, "mode": "record", "snapshot": null}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
}

/// Valeur d'une série exacte, par exemple `mimic_controller_workloads`.
fn value(text: &str, series: &str) -> Option<f64> {
    text.lines().find_map(|l| {
        l.strip_prefix(series)?
            .strip_prefix(' ')?
            .trim()
            .parse()
            .ok()
    })
}

#[tokio::test]
async fn mon_01_controller_exposes_build_info() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(config(dir.path())).await.unwrap();
    let m = metrics(&c).await;
    let prefix = format!(
        "mimic_build_info{{mimic_version=\"{}\",revision=\"",
        env!("CARGO_PKG_VERSION")
    );
    let line = m
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{prefix} absente :\n{m}"));
    assert!(line.ends_with("\"} 1"), "{line}");
}

#[tokio::test]
async fn mon_02_pod_states_are_counted_per_workload() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        pod_timeout: Duration::from_millis(100),
        pod_forget: Duration::from_millis(300),
        ..config(dir.path())
    })
    .await
    .unwrap();
    // `web` attend une capture entrante que son pod n'annonce pas : il diverge.
    c.apply_spec(
        "shop",
        "web",
        Some(MimicWorkloadSpec {
            mode: "record".into(),
            capture: Some(CaptureSpec {
                inbound: Some("metadata".into()),
            }),
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    heartbeat(&c, "http", "checkout-a").await;
    heartbeat(&c, "http", "web-a").await;

    let m = metrics(&c).await;
    let series = |wl: &str, state: &str| {
        value(
            &m,
            &format!(r#"mimic_controller_pods{{workload="shop/{wl}",state="{state}"}}"#),
        )
    };
    assert_eq!(series("checkout", "aligned"), Some(1.0), "{m}");
    assert_eq!(series("checkout", "diverging"), Some(0.0), "{m}");
    assert_eq!(series("checkout", "absent"), Some(0.0), "{m}");
    assert_eq!(series("web", "aligned"), Some(0.0), "{m}");
    assert_eq!(series("web", "diverging"), Some(1.0), "{m}");
    assert_eq!(series("web", "absent"), Some(0.0), "{m}");
    assert!(
        !m.contains("mimic_controller_pods{state="),
        "plus de série sans workload : {m}"
    );

    // Pods oubliés : les séries des deux workloads restent, à zéro.
    tokio::time::sleep(Duration::from_millis(400)).await;
    c.state().maintain().await;
    let m = metrics(&c).await;
    for wl in ["checkout", "web"] {
        for state in ["aligned", "diverging", "absent"] {
            let s = format!(r#"mimic_controller_pods{{workload="shop/{wl}",state="{state}"}}"#);
            assert_eq!(value(&m, &s), Some(0.0), "{s} :\n{m}");
        }
    }
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
async fn mon_04_controller_exposes_the_expiry_of_its_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let plain = start(config(dir.path())).await.unwrap();
    assert_eq!(
        value(
            &metrics(&plain).await,
            "mimic_controller_certificate_expiry_timestamp_seconds"
        ),
        Some(0.0),
        "0 sans TLS"
    );

    let certs = tempfile::tempdir().unwrap();
    let (cert, key) = expiring_2031();
    let files = TlsFiles::new(certs.path().join("tls.crt"), certs.path().join("tls.key"));
    std::fs::write(&files.cert_file, cert).unwrap();
    std::fs::write(&files.key_file, key).unwrap();
    let store = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        tls: Some(files),
        ..config(store.path())
    })
    .await
    .unwrap();
    heartbeat(&c, "https", "checkout-a").await;
    assert_eq!(
        value(
            &metrics(&c).await,
            "mimic_controller_certificate_expiry_timestamp_seconds"
        ),
        Some(1_925_078_400.0)
    );
}
