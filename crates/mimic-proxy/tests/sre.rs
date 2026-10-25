//! Parcours SRE de tir de charge (spec §5.8) : capture de l'entrant réglable (CAP-09,
//! CAP-10) et label `version` des métriques (ADM-04).

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use mimic_proxy::config::{InboundCapture, Mode, ShareConfig};

/// Lignes JSON de tous les fichiers `.jsonl` sous `dir`.
fn jsonl_lines(dir: &Path) -> Vec<serde_json::Value> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(jsonl_lines(&path));
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            let text = std::fs::read_to_string(&path).unwrap();
            out.extend(
                text.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| serde_json::from_str(l).unwrap()),
            );
        }
    }
    out
}

/// Contrôleur factice du workload `ns/api` : garde le corps de chaque segment publié.
async fn recording_controller() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use axum::extract::State;
    use axum::routing::post;
    type Published = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    let published = Published::default();
    let base = "/v1/workloads/ns/api";
    let app = axum::Router::new()
        .route(
            &format!("{base}/heartbeat"),
            post(|| async { axum::Json(serde_json::json!({"mode": null, "snapshot": null})) }),
        )
        .route(
            &format!("{base}/segments"),
            post(|State(p): State<Published>, body: String| async move {
                p.lock().unwrap().push(body);
                axum::http::StatusCode::CREATED
            }),
        )
        .with_state(published.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, published)
}

#[tokio::test]
async fn cap_09_metadata_keeps_only_metadata_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let app = fake_server().await;
    let saas = fake_server().await;
    let mut cfg = config(Some(app.addr), Mode::Record);
    cfg.capture.inbound = InboundCapture::Metadata;
    cfg.store.dir = Some(dir.path().join("store"));
    let (ctl, published) = recording_controller().await;
    cfg.share = Some(ShareConfig {
        url: format!("http://{ctl}"),
        allow_insecure: true,
        interval: Duration::from_millis(100),
        mode_poll: Duration::from_secs(60),
        ..ShareConfig::default()
    });
    let p = proxy(cfg).await;

    let r = client()
        .post(inbound(&p, "/echo?token=s3cr3t"))
        .header("authorization", "Bearer s3cr3t")
        .header("content-type", "application/json")
        .body(r#"{"email":"jane@example.com"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!r.bytes().await.unwrap().is_empty(), "réponse relayée");
    assert_eq!(
        app_client(&p)
            .get(saas.url("/users/7"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let list = wait_exchanges(&p, "direction=inbound", 1).await;
    assert_eq!(list.len(), 1);
    let e = &list[0];
    assert_eq!(e["method"], "POST");
    assert_eq!(e["path"], "/echo");
    assert_eq!(e["status"], 200);
    assert!(e["total_us"].as_u64().unwrap() >= e["ttfb_us"].as_u64().unwrap());
    assert_eq!(e["query"].as_array().map(Vec::len), Some(0), "{e}");
    assert_eq!(e["req_headers"].as_array().map(Vec::len), Some(0), "{e}");
    assert_eq!(e["resp_headers"].as_array().map(Vec::len), Some(0), "{e}");
    assert_eq!(e["req_body"]["kind"], "skipped", "{e}");
    assert_eq!(e["req_body"]["size"], 28);
    assert_eq!(e["resp_body"]["kind"], "skipped", "{e}");
    assert!(!e.to_string().contains("s3cr3t"));

    // Le sortant est enregistré en entier.
    let out = wait_exchanges(&p, "direction=outbound", 1).await;
    assert_eq!(out[0]["resp_body"]["kind"], "captured", "{}", out[0]);

    // Ni journal local, ni publication pour l'entrant ; le sortant y est.
    client()
        .post(admin(&p, "/share/sync"))
        .send()
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let journal = jsonl_lines(&dir.path().join("store"));
        let shared: Vec<serde_json::Value> = published
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| body.lines().map(|l| serde_json::from_str(l).unwrap()))
            .collect();
        let outbound = |l: &[serde_json::Value]| l.iter().any(|e| e["direction"] == "outbound");
        if outbound(&journal) && outbound(&shared) {
            for e in journal.iter().chain(&shared) {
                assert_eq!(e["direction"], "outbound", "{e}");
            }
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sortant ni journalisé ni publié"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn cap_10_off_keeps_nothing_but_metrics() {
    let app = fake_server().await;
    let mut cfg = config(Some(app.addr), Mode::Record);
    cfg.capture.inbound = InboundCapture::Off;
    let p = proxy(cfg).await;
    for i in 0..5 {
        let r = client()
            .get(inbound(&p, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
    }
    let m = metrics(&p).await;
    let count = |prefix: &str| -> f64 {
        m.lines()
            .filter(|l| l.starts_with(prefix) && l.contains("direction=\"inbound\""))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .sum()
    };
    assert_eq!(count("mimic_requests_total{"), 5.0, "{m}");
    // Les observations sont faites à la fin de la réponse : on les attend.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let m = metrics(&p).await;
        let n: f64 = m
            .lines()
            .filter(|l| {
                l.starts_with("mimic_request_duration_seconds_count{")
                    && l.contains("direction=\"inbound\"")
            })
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .sum();
        if n == 5.0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{n} observations\n{m}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exchanges(&p, "direction=inbound").await.is_empty());
}

/// Séries de l'exposition Prometheus (hors commentaires).
fn series(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect()
}

#[tokio::test]
async fn adm_04_every_metric_carries_the_version() {
    let app = fake_server().await;
    let mut cfg = config(Some(app.addr), Mode::Record);
    cfg.apply_env(|k| (k == "MIMIC_VERSION").then(|| "v2".to_string()))
        .unwrap();
    let p = proxy(cfg).await;
    client().get(inbound(&p, "/users/1")).send().await.unwrap();
    let m = metrics(&p).await;
    let lines = series(&m);
    assert!(lines.iter().any(|l| l.starts_with("mimic_requests_total{")));
    for l in lines {
        assert!(l.contains("version=\"v2\""), "{l}");
    }

    // Sans version connue, aucun label `version` (`mimic_version` de `mimic_build_info` est
    // un autre label).
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    client().get(inbound(&p, "/users/1")).send().await.unwrap();
    let m = metrics(&p).await;
    assert!(!m.contains("{version=") && !m.contains(",version="), "{m}");
}
