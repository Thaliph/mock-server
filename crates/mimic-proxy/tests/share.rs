//! Critères SHR-* de la spec (§5.4) : partage entre les sidecars d'un même workload.

mod common;

use std::time::{Duration, Instant};

use common::*;
use mimic_proxy::Running;
use mimic_proxy::config::{Config, Mode, ShareConfig};
use serde_json::Value;

fn shared(
    app: Option<std::net::SocketAddr>,
    mode: Mode,
    url: &str,
    instance: &str,
    workload: &str,
) -> Config {
    let mut c = config(app, mode);
    c.instance = instance.into();
    c.workload.name = workload.into();
    c.share = Some(ShareConfig {
        url: url.into(),
        interval: Duration::from_millis(100),
        retention: Duration::from_secs(3600),
        ..ShareConfig::default()
    });
    c
}

async fn paths(p: &Running) -> Vec<String> {
    let ops: Value = client()
        .get(admin(p, "/api/operations"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut out: Vec<String> = ops
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            format!(
                "{} {}",
                o["method"].as_str().unwrap(),
                o["path"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
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

#[tokio::test]
async fn shr_01_discovery_covers_all_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().display());
    let app = fake_server().await;
    let a = proxy(shared(Some(app.addr), Mode::Record, &url, "api-a", "api")).await;
    let b = proxy(shared(Some(app.addr), Mode::Record, &url, "api-b", "api")).await;
    client().get(inbound(&a, "/users/1")).send().await.unwrap();
    client()
        .get(inbound(&b, "/search?q=x"))
        .send()
        .await
        .unwrap();
    let expected = vec!["GET /search".to_string(), "GET /users/1".to_string()];
    eventually("A voit les opérations de B", || async {
        paths(&a).await == expected
    })
    .await;
    eventually("B voit les opérations de A", || async {
        paths(&b).await == expected
    })
    .await;
    let s = status(&a).await;
    assert_eq!(s["share"]["peers"], serde_json::json!(["api-b"]));
    let html = client()
        .get(admin(&a, "/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("1 pair(s)"), "{html}");
}

#[tokio::test]
async fn shr_02_replica_mocks_with_recordings_of_another() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().display());
    let saas = fake_server().await;
    let a = proxy(shared(None, Mode::Record, &url, "api-a", "api")).await;
    app_client(&a)
        .get(saas.url("/users/7"))
        .send()
        .await
        .unwrap();
    wait_exchanges(&a, "direction=outbound", 1).await;
    let b = proxy(shared(None, Mode::Mock, &url, "api-b", "api")).await;
    eventually("B a importé l'enregistrement de A", || async {
        exchanges(&b, "direction=outbound").await.len() == 1
    })
    .await;
    saas.stop();
    let r = app_client(&b)
        .get(saas.url("/users/7"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "user7");
}

#[tokio::test]
async fn shr_03_repeated_syncs_do_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().display());
    let app = fake_server().await;
    let a = proxy(shared(Some(app.addr), Mode::Record, &url, "api-a", "api")).await;
    let b = proxy(shared(Some(app.addr), Mode::Record, &url, "api-b", "api")).await;
    for i in 0..5 {
        client()
            .get(inbound(&a, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
    }
    eventually("B a importé les 5 échanges", || async {
        status(&b).await["store"]["peer"] == 5
    })
    .await;
    // Plusieurs cycles de synchronisation plus tard, toujours 5.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(status(&b).await["store"]["peer"], 5);
    assert_eq!(
        status(&a).await["store"]["peer"],
        0,
        "A ne réimporte pas ses propres échanges"
    );
}

#[tokio::test]
async fn shr_04_other_workloads_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.path().display());
    let app = fake_server().await;
    let a = proxy(shared(Some(app.addr), Mode::Record, &url, "api-a", "api")).await;
    let c = proxy(shared(Some(app.addr), Mode::Record, &url, "web-a", "web")).await;
    let b = proxy(shared(Some(app.addr), Mode::Record, &url, "api-b", "api")).await;
    client().get(inbound(&a, "/users/1")).send().await.unwrap();
    eventually("B a importé l'échange de A", || async {
        status(&b).await["store"]["peer"] == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&c).await["store"]["peer"], 0);
}

#[tokio::test]
async fn shr_05_unavailable_share_does_not_affect_traffic() {
    let file = tempfile::NamedTempFile::new().unwrap();
    // Un fichier ordinaire ne peut pas servir de répertoire : chaque synchronisation échoue.
    let url = format!("file://{}/sub", file.path().display());
    let app = fake_server().await;
    let a = proxy(shared(Some(app.addr), Mode::Record, &url, "api-a", "api")).await;
    for i in 0..3 {
        let r = client()
            .get(inbound(&a, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    eventually("échec de synchronisation compté", || async {
        metric_value(
            &metrics(&a).await,
            r#"mimic_share_sync_total{result="error"}"#,
        )
        .unwrap_or(0.0)
            >= 1.0
    })
    .await;
    assert!(status(&a).await["share"]["last_error"].is_string());
}

#[tokio::test]
async fn shr_06_without_share_nothing_waits_for_publication() {
    let app = fake_server().await;
    let a = proxy(config(Some(app.addr), Mode::Record)).await;
    for i in 0..20 {
        client()
            .get(inbound(&a, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
    }
    wait_exchanges(&a, "direction=inbound", 20).await;
    assert_eq!(a.state().store.unshared_bytes(), 0);
    assert!(a.state().store.take_unshared().is_empty());
}

#[tokio::test]
async fn shr_07_unavailable_share_keeps_a_bounded_backlog() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let url = format!("file://{}/sub", file.path().display());
    let app = fake_server().await;
    let mut cfg = shared(Some(app.addr), Mode::Record, &url, "api-a", "api");
    let max = 8 * 1024;
    cfg.share.as_mut().unwrap().max_backlog_bytes = max;
    let a = proxy(cfg).await;
    for i in 0..100 {
        let r = client()
            .get(inbound(&a, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    wait_exchanges(&a, "direction=inbound&limit=1000", 100).await;
    assert!(a.state().store.unshared_bytes() <= max);
    let dropped = metric_value(
        &metrics(&a).await,
        r#"mimic_capture_dropped_total{reason="share_backlog"}"#,
    )
    .unwrap_or(0.0);
    assert!(dropped > 0.0, "échanges jetés non comptés");
    // Les plus récents restent à publier.
    let pending = a.state().store.take_unshared();
    assert!(pending.iter().any(|e| e.path == "/users/99"));
    assert!(!pending.iter().any(|e| e.path == "/users/0"));
}

#[tokio::test]
async fn sync_endpoint_reports_when_sharing_is_disabled() {
    let p = proxy(config(None, Mode::Record)).await;
    let r = client()
        .post(admin(&p, "/share/sync"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
