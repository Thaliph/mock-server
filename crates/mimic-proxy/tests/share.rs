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
async fn sync_endpoint_reports_when_sharing_is_disabled() {
    let p = proxy(config(None, Mode::Record)).await;
    let r = client()
        .post(admin(&p, "/share/sync"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// Contrôleur factice : journalise les listes et les publications, refuse sur commande.
#[derive(Default)]
struct FakeController {
    /// `since_ms` de chaque liste demandée.
    lists: Vec<Option<i64>>,
    /// Publications : identifiant de segment, nombre de lignes, date, acceptée ou non.
    posts: Vec<(Option<String>, usize, Instant, bool)>,
    /// Segments listés.
    segments: Vec<Value>,
    refuse_posts: bool,
    /// Lectures de segment à refuser avant d'accepter.
    failing_gets: usize,
}

type Fake = std::sync::Arc<std::sync::Mutex<FakeController>>;

async fn fake_controller() -> (std::net::SocketAddr, Fake) {
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use std::collections::BTreeMap;

    let fake = Fake::default();
    let base = "/v1/workloads/ns/api";
    let app =
        axum::Router::new()
            .route(
                &format!("{base}/heartbeat"),
                post(|| async { axum::Json(serde_json::json!({"mode": null, "snapshot": null})) }),
            )
            .route(
                &format!("{base}/segments"),
                get(
                    |State(f): State<Fake>, Query(q): Query<BTreeMap<String, String>>| async move {
                        let mut f = f.lock().unwrap();
                        f.lists.push(q.get("since_ms").map(|v| v.parse().unwrap()));
                        axum::Json(Value::Array(f.segments.clone()))
                    },
                )
                .post(
                    |State(f): State<Fake>,
                     Query(q): Query<BTreeMap<String, String>>,
                     body: String| async move {
                        let mut f = f.lock().unwrap();
                        let ok = !f.refuse_posts;
                        let lines = body.lines().count();
                        f.posts
                            .push((q.get("segment").cloned(), lines, Instant::now(), ok));
                        if ok {
                            StatusCode::CREATED
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        }
                    },
                ),
            )
            .route(
                &format!("{base}/segments/{{instance}}/{{file}}"),
                get(|State(f): State<Fake>| async move {
                    let mut f = f.lock().unwrap();
                    if f.failing_gets > 0 {
                        f.failing_gets -= 1;
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    }
                    axum::http::StatusCode::OK
                }),
            )
            .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, fake)
}

/// Sidecar `api-a` rattaché au contrôleur factice.
fn piloted_by(app: Option<std::net::SocketAddr>, ctl: std::net::SocketAddr) -> Config {
    let mut c = shared(app, Mode::Record, &format!("http://{ctl}"), "api-a", "api");
    if let Some(s) = c.share.as_mut() {
        s.allow_insecure = true;
        s.mode_poll = Duration::from_secs(60);
    }
    c
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn shr_06_listing_is_incremental_after_a_successful_import() {
    let (ctl, fake) = fake_controller().await;
    let at = unix_ms() - 1_000;
    {
        let mut f = fake.lock().unwrap();
        f.segments = vec![serde_json::json!({
            "key": "api-b/01A.jsonl", "instance": "api-b", "modified_ms": at, "size": 1
        })];
        f.failing_gets = 1;
    }
    let _a = proxy(piloted_by(None, ctl)).await;
    eventually("trois listes", || async {
        fake.lock().unwrap().lists.len() >= 3
    })
    .await;
    let lists = fake.lock().unwrap().lists.clone();
    // Import échoué : la liste suivante reste complète ; ensuite, le curseur moins 60 s.
    assert_eq!(lists[0], None);
    assert_eq!(lists[1], None);
    assert_eq!(lists[2], Some(at - 60_000));
}

#[tokio::test]
async fn shr_07_publication_queue_is_bounded_and_retried_with_backoff() {
    let (ctl, fake) = fake_controller().await;
    fake.lock().unwrap().refuse_posts = true;
    let app = fake_server().await;
    let mut config = piloted_by(Some(app.addr), ctl);
    if let Some(s) = config.share.as_mut() {
        s.max_pending = 3;
    }
    let a = proxy(config).await;
    for i in 0..5 {
        client()
            .get(inbound(&a, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
    }
    eventually("quatre reprises du même segment", || async {
        let f = fake.lock().unwrap();
        let n = f.posts.len();
        n >= 4 && f.posts[n - 4..].iter().all(|p| p.0 == f.posts[n - 1].0)
    })
    .await;
    fake.lock().unwrap().refuse_posts = false;
    // Les échanges ont pu être rangés en plusieurs segments : on compte toutes les lignes acceptées.
    let accepted = || {
        let f = fake.lock().unwrap();
        f.posts.iter().filter(|p| p.3).map(|p| p.1).sum::<usize>()
    };
    eventually("publications acceptées", || async { accepted() >= 3 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(accepted(), 3, "seuls les 3 plus récents sont gardés");
    let posts = fake.lock().unwrap().posts.clone();
    let ok = posts.iter().position(|p| p.3).unwrap();
    let id = posts[ok].0.clone();
    assert!(id.is_some(), "identifiant de segment fourni");
    let retries: Vec<Instant> = posts[..ok]
        .iter()
        .filter(|p| p.0 == id)
        .map(|p| p.2)
        .collect();
    assert!(retries.len() >= 4, "{retries:?}");
    let gaps: Vec<Duration> = retries.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps[gaps.len() - 1] > gaps[0],
        "attente croissante : {gaps:?}"
    );
    let dropped = metric_value(
        &metrics(&a).await,
        r#"mimic_capture_dropped_total{reason="share_backlog"}"#,
    );
    assert_eq!(dropped, Some(2.0));
}
