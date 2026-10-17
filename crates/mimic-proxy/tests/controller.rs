//! Critères CTL-*, SNP-03 à 05 et SEC-01, SEC-05 de la spec (§5.5, §5.6, §7.1) :
//! sidecars réels pilotés par un contrôleur réel.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use mimic_controller::config::{AuthConfig, ControllerConfig, Identity, TlsFiles};
use mimic_controller::reconcile::MimicWorkloadSpec;
use mimic_proxy::Running;
use mimic_proxy::config::{Config, Mode, ShareConfig};
use serde_json::Value;

const PODS: [&str; 2] = ["api-a", "api-b"];

fn tokens() -> BTreeMap<String, Identity> {
    PODS.iter()
        .map(|p| {
            (
                format!("tok-{p}"),
                Identity {
                    namespace: "ns".into(),
                    pod: p.to_string(),
                    workload: "api".into(),
                },
            )
        })
        .collect()
}

async fn controller(dir: &Path, tls: Option<TlsFiles>) -> mimic_controller::Running {
    mimic_controller::start(ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ui_listen: "127.0.0.1:0".parse().unwrap(),
        store: format!("file://{}", dir.join("ctl").display()),
        auth: AuthConfig::Static { tokens: tokens() },
        tls,
        ..ControllerConfig::default()
    })
    .await
    .unwrap()
}

/// Sidecar `instance` piloté par le contrôleur, en HTTP de test.
fn piloted(
    app: Option<std::net::SocketAddr>,
    ctl: &mimic_controller::Running,
    dir: &Path,
    instance: &str,
) -> Config {
    let token = dir.join(format!("{instance}.token"));
    std::fs::write(&token, format!("tok-{instance}\n")).unwrap();
    let mut c = config(app, Mode::Record);
    c.instance = instance.into();
    c.share = Some(ShareConfig {
        url: format!("http://{}", ctl.api_addr()),
        interval: Duration::from_millis(100),
        mode_poll: Duration::from_millis(100),
        token_file: Some(token),
        allow_insecure: true,
        ..ShareConfig::default()
    });
    c
}

async fn status(p: &Running) -> Value {
    client()
        .get(admin(p, "/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn ui_json(ctl: &mimic_controller::Running, path: &str) -> Value {
    client()
        .get(format!("http://{}{path}", ctl.ui_addr()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "délai dépassé : {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn apply(ctl: &mimic_controller::Running, mode: &str, snapshot: Option<&str>) -> Value {
    ctl.apply_spec(
        "ns",
        "api",
        Some(MimicWorkloadSpec {
            mode: mode.into(),
            snapshot: snapshot.map(Into::into),
        }),
    )
    .await;
    ui_json(ctl, "/v1/workloads/ns/api/mode").await
}

/// Attend que chaque sidecar applique le mode et le snapshot souhaités.
async fn converged(sidecars: &[&Running], mode: &str, snapshot: &Value) {
    for p in sidecars {
        eventually(&format!("{mode} appliqué"), || async {
            let s = status(p).await;
            s["mode"] == mode && s["snapshot"] == *snapshot
        })
        .await;
    }
}

/// Réponse servie à une application derrière ce sidecar : code, type et body.
async fn served(p: &Running, url: &str) -> (u16, String, Vec<u8>) {
    let r = app_client(p).get(url).send().await.unwrap();
    let ct = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (r.status().as_u16(), ct, r.bytes().await.unwrap().to_vec())
}

#[tokio::test]
async fn ctl_01_controller_aggregates_the_workload() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let app = fake_server().await;
    let a = proxy(piloted(Some(app.addr), &ctl, dir.path(), "api-a")).await;
    let b = proxy(piloted(Some(app.addr), &ctl, dir.path(), "api-b")).await;
    client().get(inbound(&a, "/users/1")).send().await.unwrap();
    client()
        .get(inbound(&b, "/search?q=x"))
        .send()
        .await
        .unwrap();
    eventually("le contrôleur voit les deux opérations", || async {
        let ops = ui_json(&ctl, "/v1/workloads/ns/api/operations").await;
        let paths: Vec<&str> = ops
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["path"].as_str())
            .collect();
        paths.contains(&"/users/1") && paths.contains(&"/search")
    })
    .await;
    eventually("deux pods annoncés", || async {
        ui_json(&ctl, "/v1/workloads").await[0]["pods"]
            .as_array()
            .is_some_and(|p| p.len() == 2)
    })
    .await;
    // Le partage passe aussi par le contrôleur : B documente le trafic de A.
    eventually("B a importé l'échange de A", || async {
        status(&b).await["store"]["peer"].as_u64() >= Some(1)
    })
    .await;
}

#[tokio::test]
async fn ctl_02_03_snp_03_to_05_switch_and_identical_responses() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;
    let b = proxy(piloted(None, &ctl, dir.path(), "api-b")).await;

    // Seul A voit la dépendance.
    let url7 = saas.url("/users/7");
    let recorded = served(&a, &url7).await;
    assert_eq!(recorded.0, 200);
    eventually("le contrôleur a reçu l'appel sortant", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;

    // CTL-02 : bascule du workload entier, sans redémarrage.
    let desired = apply(&ctl, "mock", None).await;
    let s1 = desired["snapshot"].clone();
    assert!(s1.is_string());
    converged(&[&a, &b], "mock", &s1).await;
    eventually("pods alignés", || async {
        let wl = ui_json(&ctl, "/v1/workloads").await;
        wl[0]["aligned"] == 2 && wl[0]["ready"] == true
    })
    .await;

    // SNP-03 : B, qui n'a jamais vu l'appel, répond exactement comme A.
    let hits = saas.hits();
    assert_eq!(served(&b, &url7).await, served(&a, &url7).await);
    assert_eq!(served(&b, &url7).await.2, recorded.2);
    assert_eq!(saas.hits(), hits, "rien n'est sorti");
    let m = metrics(&b).await;
    assert_eq!(
        metric_value(&m, "mimic_mock_snapshot_exchanges"),
        Some(1.0),
        "{m}"
    );

    // CTL-03 : retour en record.
    apply(&ctl, "record", None).await;
    converged(&[&a, &b], "record", &Value::Null).await;

    // SNP-04 : un nouvel enregistrement n'entre pas dans le snapshot épinglé.
    assert_eq!(served(&a, &saas.url("/search?q=new")).await.0, 200);
    eventually("deuxième appel reçu", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 2)
    })
    .await;
    apply(&ctl, "mock", s1.as_str()).await;
    converged(&[&a, &b], "mock", &s1).await;
    assert_eq!(served(&b, &saas.url("/search?q=new")).await.0, 599);
    assert_eq!(served(&b, &url7).await.0, 200);

    // SNP-05 : contrôleur arrêté, le snapshot reste servi.
    saas.stop();
    ctl.shutdown().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(served(&b, &url7).await.2, recorded.2);
    assert_eq!(status(&b).await["mode"], "mock");
    eventually("erreur de battement visible", || async {
        status(&b).await["controller"]["last_error"].is_string()
    })
    .await;
}

#[tokio::test]
async fn sidecar_restarts_in_the_last_desired_state() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let mut cfg = piloted(None, &ctl, dir.path(), "api-a");
    cfg.store.dir = Some(dir.path().join("store-a"));
    let a = proxy(cfg.clone()).await;
    let recorded = served(&a, &saas.url("/users/7")).await;
    eventually("appel reçu", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;
    let s1 = apply(&ctl, "mock", None).await["snapshot"].clone();
    converged(&[&a], "mock", &s1).await;
    a.shutdown().await;
    ctl.shutdown().await;
    saas.stop();

    // Redémarrage sans contrôleur joignable : dernier état et snapshot rechargés du disque.
    let a = proxy(cfg).await;
    let s = status(&a).await;
    assert_eq!(s["mode"], "mock");
    assert_eq!(s["snapshot"], s1);
    assert_eq!(served(&a, &saas.url("/users/7")).await.2, recorded.2);
}

#[tokio::test]
async fn sec_01_piloted_sidecar_refuses_local_orders() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let annotations = dir.path().join("annotations");
    std::fs::write(&annotations, "mimic.platform.io/mode=\"record\"\n").unwrap();
    let mut cfg = piloted(None, &ctl, dir.path(), "api-a");
    cfg.annotations_file = Some(annotations.clone());
    let a = proxy(cfg).await;
    let r = client()
        .put(admin(&a, "/mode"))
        .body("mock")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = client().delete(admin(&a, "/store")).send().await.unwrap();
    assert_eq!(r.status(), 403);
    std::fs::write(&annotations, "mimic.platform.io/mode=\"mock\"\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(status(&a).await["mode"], "record");
}

/// CA de test et certificat serveur `localhost` signé par elle.
fn ca_and_cert(dir: &Path, name: &str) -> (std::path::PathBuf, TlsFiles) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("{name} CA"));
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let ca_file = dir.join(format!("{name}-ca.crt"));
    std::fs::write(&ca_file, ca.pem()).unwrap();
    let files = TlsFiles {
        cert_file: dir.join(format!("{name}.crt")),
        key_file: dir.join(format!("{name}.key")),
    };
    std::fs::write(&files.cert_file, cert.pem()).unwrap();
    std::fs::write(&files.key_file, key.serialize_pem()).unwrap();
    (ca_file, files)
}

fn over_https(mut c: Config, ctl: &mimic_controller::Running, ca: &Path) -> Config {
    let share = c.share.as_mut().unwrap();
    share.url = format!("https://localhost:{}", ctl.api_addr().port());
    share.allow_insecure = false;
    share.ca_file = Some(ca.to_path_buf());
    c
}

#[tokio::test]
async fn sec_05_only_the_controller_ca_is_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let (good_ca, files) = ca_and_cert(dir.path(), "controller");
    let (rogue_ca, _) = ca_and_cert(dir.path(), "rogue");
    let ctl = controller(dir.path(), Some(files)).await;
    ctl.apply_spec(
        "ns",
        "api",
        Some(MimicWorkloadSpec {
            mode: "passthrough".into(),
            snapshot: None,
        }),
    )
    .await;

    // Mauvaise CA : le battement échoue et le mode ne change pas.
    let b = proxy(over_https(
        piloted(None, &ctl, dir.path(), "api-b"),
        &ctl,
        &rogue_ca,
    ))
    .await;
    eventually("échec TLS visible", || async {
        status(&b).await["controller"]["last_error"].is_string()
    })
    .await;
    assert_eq!(status(&b).await["mode"], "record");

    // Bonne CA : le sidecar s'aligne sur l'état souhaité, en HTTPS.
    let a = proxy(over_https(
        piloted(None, &ctl, dir.path(), "api-a"),
        &ctl,
        &good_ca,
    ))
    .await;
    converged(&[&a], "passthrough", &Value::Null).await;
}

#[tokio::test]
async fn snp_06_hot_endpoint_stays_servable_beyond_the_sample_cap() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;

    // Endpoint très sollicité : bien plus d'appels que samples_per_operation (20)
    // et que snapshot.max_per_key (50). Seuls les 20 premiers gardent leur body.
    let url = saas.url("/users/1");
    let recorded = served(&a, &url).await;
    for _ in 0..99 {
        assert_eq!(served(&a, &url).await.0, 200);
    }
    eventually("le contrôleur a reçu les 100 appels", || async {
        ui_json(
            &ctl,
            "/v1/workloads/ns/api/requests?direction=outbound&limit=1000",
        )
        .await
        .as_array()
        .is_some_and(|l| l.len() == 100)
    })
    .await;

    let s1 = apply(&ctl, "mock", None).await["snapshot"].clone();
    converged(&[&a], "mock", &s1).await;
    let hits = saas.hits();
    assert_eq!(
        served(&a, &url).await,
        recorded,
        "réponse enregistrée rejouée"
    );
    assert_eq!(saas.hits(), hits, "rien n'est sorti");
}
