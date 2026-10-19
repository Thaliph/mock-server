//! Critères CTL-*, SNP-03 à 08, SHR-10 et SEC-01, SEC-05 de la spec (§5.4, §5.5, §5.6,
//! §7.1) : sidecars réels pilotés par un contrôleur réel.

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
                    // api-a est l'ancienne version, api-b la nouvelle (CRD-08).
                    labels: [(
                        "version".to_string(),
                        if *p == "api-a" { "v1" } else { "v2" }.to_string(),
                    )]
                    .into(),
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
        // Découverte recalculée dès qu'une vue la trouve périmée (§5.5.2).
        discovery_interval: Duration::ZERO,
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
            ..MimicWorkloadSpec::default()
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
async fn ctl_01_shr_10_controller_aggregates_the_workload_pods_keep_their_own() {
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
    // SHR-10 : sous contrôleur, aucun pod n'importe le trafic de l'autre ; chacun ne
    // documente que le sien, la vue du workload est celle du contrôleur.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (p, own, other) in [(&a, "/users/1", "/search"), (&b, "/search", "/users/1")] {
        let s = status(p).await;
        assert_eq!(s["store"]["peer"], 0);
        assert_eq!(s["share"]["peers"], serde_json::json!([]));
        let ops: Value = client()
            .get(admin(p, "/api/operations"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let paths: Vec<&str> = ops
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["path"].as_str())
            .collect();
        assert!(paths.contains(&own), "{paths:?}");
        assert!(!paths.contains(&other), "{paths:?}");
    }
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

/// Le garde-fou production vu de bout en bout : namespace labellisé `env=production`,
/// le sidecar ne bascule jamais en mock (E2E-04 sans cluster).
#[tokio::test]
async fn crd_07_production_namespace_never_switches_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;
    assert_eq!(served(&a, &saas.url("/users/7")).await.0, 200);
    let prod: BTreeMap<String, String> = [("env".to_string(), "production".to_string())].into();
    ctl.set_namespace_labels("ns", &prod).await;

    for mode in ["mock", "mock-partial"] {
        let desired = apply(&ctl, mode, None).await;
        assert!(desired["mode"].is_null(), "{desired}");
        assert!(desired["snapshot"].is_null(), "{desired}");
        let wl = ui_json(&ctl, "/v1/workloads").await;
        assert_eq!(wl[0]["reason"], "ProductionNamespace", "{wl}");
        assert_eq!(wl[0]["ready"], false);
    }
    // Plusieurs battements plus tard, toujours en record, et aucun snapshot construit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(status(&a).await["mode"], "record");
    let snaps = ui_json(&ctl, "/v1/workloads/ns/api/snapshots").await;
    assert_eq!(snaps.as_array().map(Vec::len), Some(0), "{snaps}");
    let hits = saas.hits();
    assert_eq!(served(&a, &saas.url("/users/7")).await.0, 200);
    assert_eq!(saas.hits(), hits + 1, "la vraie dépendance est appelée");

    // Label retiré : le mock est accepté ; label reposé : retour en record.
    ctl.set_namespace_labels("ns", &BTreeMap::new()).await;
    let desired = ui_json(&ctl, "/v1/workloads/ns/api/mode").await;
    assert_eq!(desired["mode"], "mock-partial");
    converged(&[&a], "mock-partial", &desired["snapshot"]).await;
    ctl.set_namespace_labels("ns", &prod).await;
    let desired = ui_json(&ctl, "/v1/workloads/ns/api/mode").await;
    assert_eq!(desired["mode"], "record", "{desired}");
    converged(&[&a], "record", &Value::Null).await;
}

/// Blue/green : seul le pod de la nouvelle version bascule en mock ; l'autre reste dans le
/// mode de sa configuration et n'est pas compté.
#[tokio::test]
async fn crd_08_selector_switches_only_selected_pods() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;
    let b = proxy(piloted(None, &ctl, dir.path(), "api-b")).await;
    // L'ancienne version enregistre la dépendance.
    let url7 = saas.url("/users/7");
    let recorded = served(&a, &url7).await;
    eventually("appel reçu", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;
    ctl.apply_spec(
        "ns",
        "api",
        Some(MimicWorkloadSpec {
            mode: "mock".into(),
            selector: Some(mimic_controller::crd::LabelSelector {
                match_labels: [("version".to_string(), "v2".to_string())].into(),
                ..Default::default()
            }),
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    let desired = ui_json(&ctl, "/v1/workloads/ns/api/mode").await;
    let snap = desired["snapshot"].clone();
    assert!(snap.is_string(), "{desired}");
    converged(&[&b], "mock", &snap).await;
    eventually("seul le pod sélectionné compte", || async {
        let wl = ui_json(&ctl, "/v1/workloads").await;
        wl[0]["selected_pods"] == 1 && wl[0]["aligned"] == 1 && wl[0]["ready"] == true
    })
    .await;
    let s = status(&a).await;
    assert_eq!(s["mode"], "record", "{s}");
    assert!(s["snapshot"].is_null());
    // La nouvelle version rejoue ce que l'ancienne a enregistré ; l'ancienne appelle la vraie
    // dépendance.
    let hits = saas.hits();
    assert_eq!(served(&b, &url7).await.2, recorded.2);
    assert_eq!(saas.hits(), hits);
    assert_eq!(served(&a, &url7).await.0, 200);
    assert_eq!(saas.hits(), hits + 1);
    let wl = ui_json(&ctl, "/v1/workloads").await;
    let pods = wl[0]["pods"].as_array().unwrap();
    let selected = |i: &str| pods.iter().find(|p| p["instance"] == i).unwrap()["selected"].clone();
    assert_eq!(selected("api-a"), false);
    assert_eq!(selected("api-b"), true);

    // Retour en record sans sélecteur : tout le workload.
    apply(&ctl, "record", None).await;
    converged(&[&a, &b], "record", &Value::Null).await;
    eventually("deux pods comptés", || async {
        ui_json(&ctl, "/v1/workloads").await[0]["selected_pods"] == 2
    })
    .await;
}

/// La capture de l'entrant imposée par la ressource atteint les sidecars, et l'alignement
/// en tient compte.
#[tokio::test]
async fn crd_09_capture_from_the_workload_reaches_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let app = fake_server().await;
    let a = proxy(piloted(Some(app.addr), &ctl, dir.path(), "api-a")).await;
    assert_eq!(status(&a).await["capture_inbound"], "full");
    ctl.apply_spec(
        "ns",
        "api",
        Some(MimicWorkloadSpec {
            mode: "record".into(),
            capture: Some(mimic_controller::crd::CaptureSpec {
                inbound: Some("off".into()),
            }),
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    eventually("capture off appliquée", || async {
        status(&a).await["capture_inbound"] == "off"
    })
    .await;
    eventually("pod aligné", || async {
        let wl = ui_json(&ctl, "/v1/workloads").await;
        wl[0]["aligned"] == 1 && wl[0]["ready"] == true
    })
    .await;
    client().get(inbound(&a, "/users/1")).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(exchanges(&a, "direction=inbound").await.is_empty());
    // Plus de capture imposée : retour à celle de la configuration.
    apply(&ctl, "record", None).await;
    eventually("capture full rétablie", || async {
        status(&a).await["capture_inbound"] == "full"
    })
    .await;
}

/// Le refus des WebSockets en mock imposé par la ressource atteint le sidecar : il
/// l'applique, l'annonce, et revient à sa configuration sans `spec.mock` (spec §3.4, §5.5.1).
#[tokio::test]
async fn prx_17_websocket_refusal_from_the_workload_reaches_the_sidecar() {
    use common::ws::{echo_and_close, echo_server, outbound_refused, outbound_ws};
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let server = echo_server().await;
    let a = proxy(piloted(Some(server.addr), &ctl, dir.path(), "api-a")).await;
    assert_eq!(status(&a).await["mock_websocket"], "relay");
    let spec = |websocket: Option<&str>| MimicWorkloadSpec {
        mode: "mock".into(),
        mock: websocket.map(|w| mimic_controller::crd::MockSpec {
            websocket: Some(w.into()),
        }),
        ..MimicWorkloadSpec::default()
    };
    ctl.apply_spec("ns", "api", Some(spec(Some("refuse"))))
        .await;
    eventually("refus appliqué en mock", || async {
        let s = status(&a).await;
        s["mode"] == "mock" && s["mock_websocket"] == "refuse"
    })
    .await;
    eventually("pod aligné", || async {
        let wl = ui_json(&ctl, "/v1/workloads").await;
        wl[0]["aligned"] == 1 && wl[0]["ready"] == true
    })
    .await;
    let (head, _) = outbound_refused(&a, &server, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert_eq!(server.accepted(), 0, "dépendance jointe");

    // Plus de réglage imposé : celui de la configuration, le relais.
    ctl.apply_spec("ns", "api", Some(spec(None))).await;
    eventually("relais rétabli", || async {
        status(&a).await["mock_websocket"] == "relay"
    })
    .await;
    let (ws, _) = outbound_ws(&a, &server, "/ws", &[]).await;
    echo_and_close(ws, Duration::ZERO).await;
    assert_eq!(server.connections(), 1);
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
    let files = TlsFiles::new(
        dir.join(format!("{name}.crt")),
        dir.join(format!("{name}.key")),
    );
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
            ..MimicWorkloadSpec::default()
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

#[tokio::test]
async fn snp_07_unreachable_snapshot_still_switches_to_mock() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    // Sous contrôleur, les données locales d'un pod sont ce qu'il a vu lui-même (§5.4) :
    // B enregistre l'appel dans son journal local, puis s'arrête.
    let with_store = |mut c: Config| {
        c.store.dir = Some(dir.path().join("store-b"));
        c
    };
    let url7 = saas.url("/users/7");
    let b = proxy(with_store(piloted(None, &ctl, dir.path(), "api-b"))).await;
    assert_eq!(served(&b, &url7).await.0, 200);
    eventually("appel reçu", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;
    b.shutdown().await;
    let s1 = apply(&ctl, "mock", None).await["snapshot"].clone();
    assert!(s1.is_string());

    // Snapshot devenu illisible : B, qui redémarre, ne peut pas le télécharger.
    let file = dir
        .path()
        .join("ctl/ns/api/snapshots")
        .join(format!("{}.jsonl", s1.as_str().unwrap()));
    std::fs::remove_file(&file).unwrap();
    let b = proxy(with_store(piloted(None, &ctl, dir.path(), "api-b"))).await;
    eventually("B en mock malgré l'échec", || async {
        let s = status(&b).await;
        s["mode"] == "mock" && s["controller"]["last_error"].is_string()
    })
    .await;
    assert_eq!(status(&b).await["snapshot"], Value::Null);
    // Données locales : l'échange que B a lui-même enregistré.
    let hits = saas.hits();
    eventually("servi depuis les données locales", || async {
        served(&b, &url7).await.0 == 200
    })
    .await;
    assert_eq!(saas.hits(), hits, "rien n'est sorti");
}

/// Forme de mock au-delà de `share.max_snapshot_bytes`, telle quelle ou une fois
/// décompressée : refusée sans rien charger ; sous la borne, chargée (SNP-08).
#[tokio::test]
async fn snp_08_mock_form_beyond_the_bound_is_refused() {
    assert_eq!(ShareConfig::default().max_snapshot_bytes, 64 * 1024 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let bounded = |instance: &str, max: usize| {
        let mut c = piloted(None, &ctl, dir.path(), instance);
        if let Some(s) = c.share.as_mut() {
            s.max_snapshot_bytes = max;
        }
        c
    };
    // Réponse très compressible : la forme compressée tient sous 20 000 octets, pas la
    // forme décompressée.
    let a = proxy(bounded("api-a", 20_000)).await;
    let b = proxy(bounded("api-b", 64)).await;
    let r = app_client(&a)
        .post(saas.url("/echo"))
        .body(vec![b'x'; 100 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    eventually("appel reçu", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;
    let s1 = apply(&ctl, "mock", None).await["snapshot"].clone();
    assert!(s1.is_string());
    for p in [&a, &b] {
        eventually("forme de mock refusée", || async {
            let s = status(p).await;
            s["mode"] == "mock"
                && s["controller"]["last_error"]
                    .as_str()
                    .is_some_and(|e| e.contains("share.max_snapshot_bytes"))
        })
        .await;
        assert_eq!(status(p).await["snapshot"], Value::Null);
    }
    // A reçoit la forme compressée sous la borne, B la refuse dès le téléchargement.
    let error = |s: Value| s["controller"]["last_error"].as_str().unwrap().to_string();
    let (ea, eb) = (error(status(&a).await), error(status(&b).await));
    assert!(ea.contains("une fois décompressée"), "{ea}");
    assert!(eb.contains("réponse au-delà"), "{eb}");
    // Sous la borne : chargée.
    a.shutdown().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;
    converged(&[&a], "mock", &s1).await;
}

/// Les exemples par endpoint viennent de segments déjà redactés par le sidecar (§5.5.3).
#[tokio::test]
async fn ctl_20_examples_hold_only_redacted_values() {
    use mimic_core::model::{Body, Exchange};
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let a = proxy(piloted(None, &ctl, dir.path(), "api-a")).await;
    let secret = "s3cr3t-t0ken-ctl20";
    let email = "alice.ctl20@example.com";
    let r = app_client(&a)
        .post(saas.url("/echo"))
        .bearer_auth(secret)
        .json(&serde_json::json!({"email": email, "n": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    eventually("le contrôleur a reçu l'appel", || async {
        ui_json(&ctl, "/v1/workloads/ns/api/requests?direction=outbound")
            .await
            .as_array()
            .is_some_and(|l| l.len() == 1)
    })
    .await;
    ctl.state().maintain().await;
    let raw = std::fs::read(dir.path().join("ctl/ns/api/exemplars.jsonl")).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.contains(secret), "{text}");
    let lines: Vec<Exchange> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let e = lines.iter().find(|e| e.path == "/echo").expect("exemple");
    let auth = e.req_header("authorization").unwrap();
    assert!(auth.starts_with("<redacted:"), "{auth}");
    for body in [&e.req_body, &e.resp_body] {
        let Body::Captured { data } = body else {
            panic!("body rejouable attendu : {body:?}");
        };
        let data = String::from_utf8_lossy(data);
        assert!(!data.contains(email), "{data}");
        assert!(data.contains("<redacted:"), "{data}");
    }
}

/// Contrôleur d'une version antérieure : battement et snapshot complet seulement (pas de
/// `…/snapshots/{id}/mock`, donc `404`), comme avant la forme de mock (§5.6).
async fn old_controller(snapshots: std::path::PathBuf, id: String) -> std::net::SocketAddr {
    use axum::extract::Path as UrlPath;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/v1/workloads/ns/api/heartbeat",
            post(move || {
                let id = id.clone();
                async move { axum::Json(serde_json::json!({"mode": "mock", "snapshot": id})) }
            }),
        )
        .route(
            "/v1/workloads/ns/api/snapshots/{id}",
            get(move |UrlPath(id): UrlPath<String>| {
                let file = snapshots.join(format!("{id}.jsonl"));
                async move { std::fs::read(file).map_err(|_| StatusCode::NOT_FOUND) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn shr_11_two_sidecars_load_the_same_mock_form_and_serve_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let ctl = controller(dir.path(), None).await;
    let saas = fake_server().await;
    let with_store = |mut c: Config, store: &str| {
        c.store.dir = Some(dir.path().join(store));
        c
    };
    let a = proxy(with_store(
        piloted(None, &ctl, dir.path(), "api-a"),
        "store-a",
    ))
    .await;
    let b = proxy(with_store(
        piloted(None, &ctl, dir.path(), "api-b"),
        "store-b",
    ))
    .await;

    // A seul appelle la dépendance, sur 60 chemins de même forme : bien au-delà de
    // samples_per_operation (20), les derniers n'ont plus de body.
    let urls: Vec<String> = (0..60).map(|i| saas.url(&format!("/users/{i}"))).collect();
    let mut recorded = vec![];
    for u in &urls {
        recorded.push(served(&a, u).await);
    }
    eventually("le contrôleur a reçu les 60 appels", || async {
        ui_json(
            &ctl,
            "/v1/workloads/ns/api/requests?direction=outbound&limit=1000",
        )
        .await
        .as_array()
        .is_some_and(|l| l.len() == 60)
    })
    .await;
    let s1 = apply(&ctl, "mock", None).await["snapshot"].clone();
    converged(&[&a, &b], "mock", &s1).await;
    let id = s1.as_str().unwrap().to_string();

    // Même forme de mock, octet pour octet, sur les deux pods : celle du contrôleur.
    let cached = |store: &str| {
        std::fs::read(
            dir.path()
                .join(store)
                .join("controller/snapshots")
                .join(format!("{id}.mock.json")),
        )
        .unwrap()
    };
    let form = cached("store-a");
    assert_eq!(cached("store-b"), form);
    let from_ctl = client()
        .get(format!(
            "http://{}/v1/workloads/ns/api/snapshots/{id}/mock",
            ctl.api_addr()
        ))
        .bearer_auth("tok-api-b")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(from_ctl.to_vec(), form);
    let full_file = dir
        .path()
        .join("ctl/ns/api/snapshots")
        .join(format!("{id}.jsonl"));
    let full = std::fs::read(&full_file).unwrap();
    assert!(
        form.len() < full.len(),
        "{} contre {}",
        form.len(),
        full.len()
    );

    // Même réponse sur les deux pods, avec ou sans body enregistré pour ce chemin, et la
    // réponse enregistrée là où elle a gardé son body.
    let hits = saas.hits();
    let mut replayed = 0;
    let mut answers = vec![];
    for (u, rec) in urls.iter().zip(&recorded) {
        let ra = served(&a, u).await;
        assert_eq!(ra.0, 200, "{u}");
        assert_eq!(served(&b, u).await, ra, "{u}");
        if ra == *rec {
            replayed += 1;
        }
        answers.push(ra);
    }
    assert!(replayed >= 20, "{replayed} réponses enregistrées rejouées");
    assert!(
        replayed < 60,
        "les chemins sans body sont servis par leur template"
    );
    assert_eq!(saas.hits(), hits, "rien n'est sorti");

    // Contrôleur d'une version antérieure : le sidecar télécharge le snapshot complet et en
    // déduit la même forme de mock.
    let old = old_controller(full_file.parent().unwrap().to_path_buf(), id.clone()).await;
    let mut cfg = with_store(piloted(None, &ctl, dir.path(), "api-c"), "store-c");
    if let Some(share) = cfg.share.as_mut() {
        share.url = format!("http://{old}");
    }
    let c = proxy(cfg).await;
    converged(&[&c], "mock", &s1).await;
    assert_eq!(cached("store-c"), form);
    for (u, ra) in urls.iter().zip(&answers) {
        assert_eq!(served(&c, u).await, *ra, "{u}");
    }
    assert_eq!(saas.hits(), hits, "rien n'est sorti");
}
