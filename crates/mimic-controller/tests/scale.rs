//! Critères CTL-06 à CTL-19 et CTL-22 de la spec (§5.5.2, §5.5.3) : tenue en charge du contrôleur,
//! exemples par endpoint et historique court.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimic_controller::config::{AuthConfig, ControllerConfig, Identity};
use mimic_controller::reconcile::MimicWorkloadSpec;
use mimic_controller::{Running, start, start_with};
use mimic_core::compact::hour_of;
use serde_json::{Value, json};

const WL: &str = "/v1/workloads/shop/checkout";
const DAY_MS: u64 = 24 * 3600 * 1000;

fn config(dir: &Path) -> ControllerConfig {
    let pods = [
        ("checkout-a", "checkout"),
        ("checkout-b", "checkout"),
        ("checkout-c", "checkout"),
        ("web-a", "web"),
    ];
    let tokens: BTreeMap<String, Identity> = pods
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

fn url(c: &Running, path: &str) -> String {
    format!("http://{}{path}", c.api_addr())
}

fn ui(c: &Running, path: &str) -> String {
    format!("http://{}{path}", c.ui_addr())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Échange au format du store ; `body` lui donne un body de réponse capturé.
fn exchange(id: &str, direction: &str, path: &str, status: u16, body: bool) -> Value {
    json!({
        "id": id, "direction": direction, "started_at_ms": 1, "scheme": "http",
        "authority": if direction == "outbound" { "api.saas.io:443" } else { "checkout:8080" },
        "method": "GET", "path": path, "query": [],
        "req_headers": [], "req_body": {"kind": "empty"},
        "status": status, "resp_headers": [],
        "resp_body": if body { json!({"kind": "captured", "data": "eyJpZCI6N30="}) } else { json!({"kind": "empty"}) },
        "ttfb_us": 1000, "total_us": 2000, "trace_id": null
    })
}

fn jsonl(exchanges: &[Value]) -> String {
    exchanges.iter().map(|e| format!("{e}\n")).collect()
}

async fn post_segment(
    c: &Running,
    instance: &str,
    segment: Option<&str>,
    exchanges: &[Value],
) -> reqwest::Response {
    let workload = instance.rsplit_once('-').unwrap().0;
    let mut path = format!("/v1/workloads/shop/{workload}/segments?instance={instance}");
    if let Some(s) = segment {
        path.push_str(&format!("&segment={s}"));
    }
    client()
        .post(url(c, &path))
        .bearer_auth(format!("tok-{instance}"))
        .body(jsonl(exchanges))
        .send()
        .await
        .unwrap()
}

async fn push(c: &Running, instance: &str, exchanges: &[Value]) {
    let r = post_segment(c, instance, None, exchanges).await;
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
}

/// `GET …/segments` vu par checkout-a, avec la query donnée.
async fn segments(c: &Running, query: &str) -> Vec<Value> {
    let r = client()
        .get(url(c, &format!("{WL}/segments{query}")))
        .bearer_auth("tok-checkout-a")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

fn keys(list: &[Value]) -> Vec<String> {
    list.iter()
        .map(|s| s["key"].as_str().unwrap().to_string())
        .collect()
}

async fn get_json(c: &Running, path: &str) -> Value {
    let r = client().get(ui(c, path)).send().await.unwrap();
    assert!(r.status().is_success(), "{path}: {}", r.status());
    r.json().await.unwrap()
}

async fn get_text(c: &Running, path: &str) -> String {
    client()
        .get(ui(c, path))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn heartbeat(c: &Running, instance: &str) -> reqwest::Response {
    let workload = instance.rsplit_once('-').unwrap().0;
    client()
        .post(url(c, &format!("/v1/workloads/shop/{workload}/heartbeat")))
        .bearer_auth(format!("tok-{instance}"))
        .json(&json!({"instance": instance, "mode": "record", "snapshot": null}))
        .send()
        .await
        .unwrap()
}

async fn pods(c: &Running) -> Vec<String> {
    let wl = get_json(c, "/v1/workloads").await;
    let checkout = wl
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "checkout")
        .unwrap();
    checkout["pods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["instance"].as_str().unwrap().to_string())
        .collect()
}

/// Identifiants des requêtes enregistrées d'un workload, les plus récentes d'abord.
async fn request_ids(c: &Running, workload: &str, direction: &str) -> Vec<String> {
    get_json(
        c,
        &format!("/v1/workloads/shop/{workload}/requests?direction={direction}"),
    )
    .await
    .as_array()
    .unwrap()
    .iter()
    .map(|r| r["id"].as_str().unwrap().to_string())
    .collect()
}

/// Dépose un segment sur le support comme s'il y avait été écrit à `at_ms`.
fn write_segment(dir: &Path, wl: &str, instance: &str, name: &str, lines: &[Value], at_ms: u64) {
    let d = dir.join(format!("shop/{wl}/exchanges/{instance}"));
    std::fs::create_dir_all(&d).unwrap();
    let path = d.join(name);
    std::fs::write(&path, jsonl(lines)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(at_ms))
        .unwrap();
}

/// Fichiers rangés sous `exchanges/` d'un workload, chemins relatifs triés.
fn exchange_files(dir: &Path, wl: &str) -> Vec<String> {
    let root = dir.join(format!("shop/{wl}/exchanges"));
    let mut out = vec![];
    let mut stack: Vec<PathBuf> = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                let rel = e.path().strip_prefix(&root).unwrap().display().to_string();
                out.push(rel);
            }
        }
    }
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

async fn operation_paths(c: &Running) -> Vec<String> {
    let ops = get_json(c, &format!("{WL}/operations")).await;
    let mut out: Vec<String> = ops
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["path"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn ctl_06_segments_index_is_built_by_a_single_listing() {
    let dir = tempfile::tempdir().unwrap();
    let at = now_ms() - 60_000;
    let e = exchange("checkout-a-1", "inbound", "/a", 200, false);
    write_segment(dir.path(), "checkout", "checkout-a", "01A.jsonl", &[e], at);
    let w = exchange("web-a-1", "inbound", "/w", 200, false);
    write_segment(dir.path(), "web", "web-a", "01W.jsonl", &[w], at);
    let c = start(config(dir.path())).await.unwrap();

    assert_eq!(keys(&segments(&c, "").await), ["checkout-a/01A.jsonl"]);
    let names: Vec<Value> = get_json(&c, "/v1/workloads")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].clone())
        .collect();
    assert_eq!(names, [json!("checkout"), json!("web")]);

    // Dépôts d'un autre processus : le support n'est plus relu.
    let late = exchange("checkout-b-1", "inbound", "/late", 200, false);
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01B.jsonl",
        &[late],
        at,
    );
    write_segment(dir.path(), "late", "late-a", "01L.jsonl", &[], at);
    assert_eq!(keys(&segments(&c, "").await), ["checkout-a/01A.jsonl"]);
    assert_eq!(
        get_json(&c, "/v1/workloads")
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );

    push(
        &c,
        "checkout-c",
        &[exchange("checkout-c-1", "inbound", "/c", 200, false)],
    )
    .await;
    let list = keys(&segments(&c, "").await);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0], "checkout-a/01A.jsonl");
    assert!(list[1].starts_with("checkout-c/"), "{list:?}");
}

#[tokio::test]
async fn ctl_07_listing_filters_by_retention_and_since() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    // Rétention par défaut : 24 heures (CTL-21).
    let (old, mid, new) = (now - 2 * DAY_MS, now - 2 * 3600 * 1000, now - 1_000);
    let e = |id: &str| exchange(id, "inbound", "/a", 200, false);
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01O.jsonl",
        &[e("o")],
        old,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01M.jsonl",
        &[e("m")],
        mid,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01N.jsonl",
        &[e("n")],
        new,
    );
    let c = start(config(dir.path())).await.unwrap();

    let all = segments(&c, "").await;
    assert_eq!(keys(&all), ["checkout-b/01M.jsonl", "checkout-a/01N.jsonl"]);
    assert_eq!(all[0]["modified_ms"], mid);
    assert_eq!(all[0]["instance"], "checkout-b");
    assert_eq!(
        keys(&segments(&c, &format!("?since_ms={}", new - 10)).await),
        ["checkout-a/01N.jsonl"]
    );
    assert_eq!(
        keys(&segments(&c, &format!("?since_ms={mid}")).await),
        ["checkout-b/01M.jsonl", "checkout-a/01N.jsonl"]
    );
    assert_eq!(
        keys(&segments(&c, "?since_ms=0").await),
        ["checkout-b/01M.jsonl", "checkout-a/01N.jsonl"]
    );
    assert!(
        segments(&c, &format!("?since_ms={}", now + 60_000))
            .await
            .is_empty()
    );
}

/// Support dont les lectures de segments restent bloquées tant que `blocked` est vrai.
#[derive(Debug)]
struct Gate {
    inner: Arc<dyn object_store::ObjectStore>,
    blocked: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
}

impl std::fmt::Display for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gate({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for Gate {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if location.as_ref().contains("/exchanges/") {
            while self.blocked.load(Ordering::SeqCst) {
                self.entered.notify_one();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn ctl_08_heartbeats_never_wait_for_storage_or_computation() {
    let dir = tempfile::tempdir().unwrap();
    let e = exchange("checkout-a-1", "inbound", "/slow", 200, false);
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01A.jsonl",
        &[e],
        now_ms(),
    );
    let blocked = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let local = object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap();
    let store = Arc::new(Gate {
        inner: Arc::new(local),
        blocked: blocked.clone(),
        entered: entered.clone(),
    });
    let c = start_with(config(dir.path()), store).await.unwrap();
    c.apply_spec(
        "shop",
        "checkout",
        Some(MimicWorkloadSpec {
            mode: "record".into(),
            snapshot: None,
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;

    blocked.store(true, Ordering::SeqCst);
    let ops_url = ui(&c, &format!("{WL}/operations"));
    let view =
        tokio::spawn(async move { client().get(ops_url).send().await.unwrap().text().await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("la vue lit le support");

    let quick = Duration::from_secs(2);
    let hb = tokio::time::timeout(quick, heartbeat(&c, "checkout-a"))
        .await
        .expect("battement pendant la lecture du support");
    // Le sel du workload n'y est que s'il a pu être lu à temps (au plus 250 ms, CTL-49).
    let mut reply = hb.json::<Value>().await.unwrap();
    reply.as_object_mut().unwrap().remove("redact_salt");
    assert_eq!(reply, json!({"mode": "record", "snapshot": null}));
    tokio::time::timeout(quick, get_json(&c, "/v1/workloads"))
        .await
        .expect("liste des workloads pendant la lecture du support");
    tokio::time::timeout(quick, get_text(&c, "/metrics"))
        .await
        .expect("métriques pendant la lecture du support");
    assert!(!view.is_finished(), "la vue attend toujours le support");

    blocked.store(false, Ordering::SeqCst);
    let body = view.await.unwrap().unwrap();
    assert!(body.contains("/slow"), "{body}");
}

#[tokio::test]
async fn ctl_09_vanished_pods_are_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        pod_timeout: Duration::from_millis(50),
        pod_forget: Duration::from_millis(300),
        ..config(dir.path())
    })
    .await
    .unwrap();
    heartbeat(&c, "checkout-a").await;
    assert_eq!(pods(&c).await, ["checkout-a"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    // Un nouveau pod remplace l'ancien (déploiement) : l'ancien est oublié.
    heartbeat(&c, "checkout-b").await;
    assert_eq!(pods(&c).await, ["checkout-b"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    c.state().maintain().await;
    assert!(pods(&c).await.is_empty());
    let m = get_text(&c, "/metrics").await;
    assert!(
        m.contains(r#"mimic_controller_pods{workload="shop/checkout",state="absent"} 0"#),
        "{m}"
    );
}

#[tokio::test]
async fn ctl_10_retention_gc_removes_old_segments_only() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let out = |id: &str| exchange(id, "outbound", "/users/7", 200, true);
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01O.jsonl",
        &[out("o1")],
        now - 2 * DAY_MS,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01P.jsonl",
        &[out("o2")],
        now - 3 * DAY_MS,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01N.jsonl",
        &[out("n1")],
        now - 1_000,
    );
    let c = start(config(dir.path())).await.unwrap();
    c.apply_spec(
        "shop",
        "checkout",
        Some(MimicWorkloadSpec {
            mode: "mock".into(),
            snapshot: None,
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    let desired = get_json(&c, &format!("{WL}/mode")).await;
    let id = desired["snapshot"].as_str().unwrap().to_string();

    c.state().maintain().await;
    assert_eq!(
        exchange_files(dir.path(), "checkout"),
        ["checkout-a/01N.jsonl"]
    );
    assert_eq!(keys(&segments(&c, "").await), ["checkout-a/01N.jsonl"]);
    assert!(
        dir.path()
            .join(format!("shop/checkout/snapshots/{id}.jsonl"))
            .exists()
    );
    assert!(dir.path().join("shop/checkout/desired.json").exists());
    assert_eq!(get_json(&c, &format!("{WL}/mode")).await, desired);
}

#[tokio::test]
async fn ctl_11_raw_segments_of_a_past_hour_are_compacted() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let (hour, start_ms, end_ms) = hour_of(now as i64 - 3 * 3600 * 1000);
    let (t1, t2) = (start_ms as u64 + 1_000, start_ms as u64 + 2_000);
    let a = |id: &str, status: u16| exchange(id, "outbound", "/a", status, true);
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01A.jsonl",
        &[
            a("checkout-a-1", 200),
            a("checkout-a-2", 200),
            a("checkout-a-3", 200),
        ],
        t1,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01B.jsonl",
        &[a("checkout-a-2", 200), a("checkout-b-1", 404)],
        t2,
    );
    write_segment(
        dir.path(),
        "checkout",
        "checkout-a",
        "01Z.jsonl",
        &[a("checkout-a-9", 200)],
        now - 1_000,
    );
    let c = start(ControllerConfig {
        samples_per_operation: 2,
        ..config(dir.path())
    })
    .await
    .unwrap();

    c.state().maintain().await;
    let compacted = format!("_hour/{hour}.jsonl");
    assert_eq!(
        exchange_files(dir.path(), "checkout"),
        [compacted.clone(), "checkout-a/01Z.jsonl".to_string()]
    );
    let list = segments(&c, "").await;
    assert_eq!(
        keys(&list),
        [compacted.clone(), "checkout-a/01Z.jsonl".to_string()]
    );
    assert_eq!(list[0]["instance"], "_hour");
    assert_eq!(list[0]["modified_ms"], end_ms - 1);
    // Un curseur antérieur à l'un de ses échanges liste encore le segment horaire.
    assert_eq!(
        keys(&segments(&c, &format!("?since_ms={t2}")).await)[0],
        compacted
    );

    let body = client()
        .get(url(&c, &format!("{WL}/segments/{compacted}")))
        .bearer_auth("tok-checkout-a")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let lines: Vec<Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let ids: Vec<&str> = lines.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "checkout-a-1",
            "checkout-a-2",
            "checkout-a-3",
            "checkout-b-1"
        ]
    );
    let kinds: Vec<&str> = lines
        .iter()
        .map(|l| l["resp_body"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["captured", "captured", "skipped", "captured"]);

    // Une nouvelle passe ne change rien.
    c.state().maintain().await;
    assert_eq!(exchange_files(dir.path(), "checkout").len(), 2);
}

#[tokio::test]
async fn ctl_12_bodies_are_capped_per_operation_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        samples_per_operation: 2,
        max_exchanges_per_workload: 4,
        ..config(dir.path())
    })
    .await
    .unwrap();
    let list: Vec<Value> = (1..=4)
        .map(|i| exchange(&format!("checkout-a-{i}"), "outbound", "/a", 200, true))
        .collect();
    push(&c, "checkout-a", &list).await;
    let kinds = |list: Value| -> Vec<(String, String)> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap().to_string(),
                    r["resp_body"]["kind"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let path = format!("{WL}/requests?direction=outbound");
    let mut got = kinds(get_json(&c, &path).await);
    got.sort();
    assert_eq!(
        got,
        [
            ("checkout-a-1".into(), "captured".into()),
            ("checkout-a-2".into(), "captured".into()),
            ("checkout-a-3".into(), "skipped".into()),
            ("checkout-a-4".into(), "skipped".into()),
        ]
    );
    // L'éviction de checkout-a-1 libère une place pour le suivant.
    push(
        &c,
        "checkout-a",
        &[exchange("checkout-a-5", "outbound", "/a", 200, true)],
    )
    .await;
    let mut got = kinds(get_json(&c, &path).await);
    got.sort();
    assert_eq!(got[0], ("checkout-a-2".into(), "captured".into()));
    assert_eq!(got[3], ("checkout-a-5".into(), "captured".into()));
}

#[tokio::test]
async fn ctl_13_memory_is_bounded_globally_and_on_reload() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        max_exchanges_per_workload: 5,
        max_exchanges_total: 6,
        ..config(dir.path())
    })
    .await
    .unwrap();
    // Workloads déjà vus : leurs échanges sont en mémoire dès l'ingestion.
    assert!(request_ids(&c, "checkout", "inbound").await.is_empty());
    assert!(request_ids(&c, "web", "inbound").await.is_empty());
    for i in 1..=5 {
        let e = exchange(&format!("checkout-a-{i}"), "inbound", "/a", 200, false);
        push(&c, "checkout-a", &[e]).await;
        // Dates d'écriture distinctes : la relecture part du plus récent.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let web: Vec<Value> = (1..=3)
        .map(|i| exchange(&format!("web-a-{i}"), "inbound", "/w", 200, false))
        .collect();
    push(&c, "web-a", &web).await;
    // Le plus gros workload rend ses plus anciens échanges.
    assert_eq!(
        request_ids(&c, "checkout", "inbound").await,
        ["checkout-a-5", "checkout-a-4", "checkout-a-3"]
    );
    assert_eq!(request_ids(&c, "web", "inbound").await.len(), 3);
    let m = get_text(&c, "/metrics").await;
    assert!(m.contains("mimic_controller_exchanges 6"), "{m}");
    c.shutdown().await;

    // Après le redémarrage, l'ingestion ne relit pas l'historique ; la première vue relit
    // seulement les plus récents.
    let c = start(ControllerConfig {
        max_exchanges_per_workload: 2,
        ..config(dir.path())
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let e = exchange("checkout-a-6", "inbound", "/a", 200, false);
    push(&c, "checkout-a", &[e]).await;
    let m = get_text(&c, "/metrics").await;
    assert!(m.contains("mimic_controller_exchanges 0"), "{m}");
    assert_eq!(
        request_ids(&c, "checkout", "inbound").await,
        ["checkout-a-6", "checkout-a-5"]
    );
}

#[tokio::test]
async fn ctl_14_discovery_is_recomputed_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        discovery_interval: Duration::from_secs(2),
        ..config(dir.path())
    })
    .await
    .unwrap();
    push(
        &c,
        "checkout-a",
        &[exchange("checkout-a-1", "inbound", "/a", 200, false)],
    )
    .await;
    let computed = Instant::now();
    assert_eq!(operation_paths(&c).await, ["/a"]);
    push(
        &c,
        "checkout-a",
        &[exchange("checkout-a-2", "inbound", "/b", 200, false)],
    )
    .await;
    // Dans l'intervalle : le dernier document calculé, sans recalcul.
    assert_eq!(operation_paths(&c).await, ["/a"]);
    assert!(
        computed.elapsed() < Duration::from_secs(2),
        "test trop lent"
    );
    eventually("nouvelle opération découverte", || async {
        operation_paths(&c).await == ["/a", "/b"]
    })
    .await;
    assert!(computed.elapsed() >= Duration::from_secs(2));
}

#[tokio::test]
async fn ctl_15_republished_segment_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(config(dir.path())).await.unwrap();
    let id = "01JCDE0123456789ABCDEFGHJK";
    let e = [exchange("checkout-a-1", "inbound", "/a", 200, false)];
    let first = post_segment(&c, "checkout-a", Some(id), &e).await;
    assert_eq!(first.status(), 201);
    let key = first.text().await.unwrap();
    assert_eq!(key, format!("checkout-a/{id}.jsonl"));
    let again = post_segment(&c, "checkout-a", Some(id), &e).await;
    assert_eq!(again.status(), 200);
    assert_eq!(again.text().await.unwrap(), key);
    assert_eq!(
        exchange_files(dir.path(), "checkout"),
        std::slice::from_ref(&key)
    );
    assert_eq!(keys(&segments(&c, "").await), [key]);
    assert_eq!(
        request_ids(&c, "checkout", "inbound").await,
        ["checkout-a-1"]
    );
    let m = get_text(&c, "/metrics").await;
    assert!(
        m.contains(r#"mimic_controller_segments_total{result="ok"} 1"#),
        "{m}"
    );
    assert!(
        m.contains(r#"mimic_controller_segments_total{result="duplicate"} 1"#),
        "{m}"
    );
    // Identifiant invalide ou réservé.
    for bad in ["..", "_hour", "a%2Fb"] {
        let r = post_segment(&c, "checkout-a", Some(bad), &e).await;
        assert_eq!(r.status(), 400, "{bad}");
    }
}

/// Échange daté de `at_ms`.
fn at(mut e: Value, at_ms: u64) -> Value {
    e["started_at_ms"] = json!(at_ms);
    e
}

/// (identifiant, chemin, nature du body de réponse) de chaque ligne JSONL.
fn summarize(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (
                v["id"].as_str().unwrap().to_string(),
                v["path"].as_str().unwrap().to_string(),
                v["resp_body"]["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Lignes de `exemplars.jsonl` d'un workload.
fn exemplars(dir: &Path, wl: &str) -> Vec<(String, String, String)> {
    let path = dir.join(format!("shop/{wl}/exemplars.jsonl"));
    summarize(&std::fs::read_to_string(path).unwrap_or_default())
}

/// Bascule le workload en mock et rend les lignes de son snapshot, triées.
async fn mock_snapshot(c: &Running, dir: &Path, wl: &str) -> Vec<(String, String, String)> {
    c.apply_spec(
        "shop",
        wl,
        Some(MimicWorkloadSpec {
            mode: "mock".into(),
            snapshot: None,
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    let desired = get_json(c, &format!("/v1/workloads/shop/{wl}/mode")).await;
    let id = desired["snapshot"].as_str().expect("snapshot construit");
    let text =
        std::fs::read_to_string(dir.join(format!("shop/{wl}/snapshots/{id}.jsonl"))).unwrap();
    let mut out = summarize(&text);
    out.sort();
    out
}

fn metric(m: &str, name: &str) -> Option<f64> {
    m.lines()
        .find(|l| l.split_whitespace().next() == Some(name))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// Deux identifiants : le second, plus récent, perd son body à la compaction avec
/// `samples_per_operation: 1` (le réservoir garde la plus petite priorité, §5.2).
fn compaction_pair() -> (String, String) {
    use mimic_core::sample::priority;
    for i in 0..1000 {
        let (old, new) = (format!("checkout-b-h{i}"), format!("checkout-b-h{}", i + 1));
        if priority(&old) < priority(&new) {
            return (old, new);
        }
    }
    unreachable!()
}

fn captured(id: &str, path: &str) -> (String, String, String) {
    (id.to_string(), path.to_string(), "captured".to_string())
}

#[tokio::test]
async fn ctl_17_examples_survive_retention_compaction_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let out = |id: &str, path: &str, ms: u64| at(exchange(id, "outbound", path, 200, true), ms);
    // Déposé avant le démarrage, hors rétention : jamais proposé aux exemples.
    let old_at = now - 2 * DAY_MS;
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01OLD.jsonl",
        &[out("checkout-b-old", "/old", old_at)],
        old_at,
    );
    // Heure révolue : le plus récent des deux appels perd son body à la compaction.
    let (_, hour_start, _) = hour_of(now as i64 - 3 * 3600 * 1000);
    let (h_old, h_new) = compaction_pair();
    let t = hour_start as u64 + 1_000;
    write_segment(
        dir.path(),
        "checkout",
        "checkout-b",
        "01HOUR.jsonl",
        &[out(&h_old, "/hour", t), out(&h_new, "/hour", t + 1)],
        t + 2,
    );
    let config = || ControllerConfig {
        samples_per_operation: 1,
        ..config(dir.path())
    };
    let c = start(config()).await.unwrap();
    // `/rare` : appelé une seule fois, ingéré par ce processus.
    push(
        &c,
        "checkout-a",
        &[out("checkout-a-rare", "/rare", now - 5_000)],
    )
    .await;

    c.state().maintain().await;
    let files = exchange_files(dir.path(), "checkout");
    assert!(
        !files
            .iter()
            .any(|f| f.contains("01OLD") || f.contains("01HOUR")),
        "segments supprimés et compactés : {files:?}"
    );
    let hour = files.iter().find(|f| f.starts_with("_hour/")).unwrap();
    let compacted = summarize(
        &std::fs::read_to_string(dir.path().join("shop/checkout/exchanges").join(hour)).unwrap(),
    );
    assert!(
        compacted.contains(&(h_new.clone(), "/hour".into(), "skipped".into())),
        "le plus récent a perdu son body dans le segment horaire : {compacted:?}"
    );
    let mut ex = exemplars(dir.path(), "checkout");
    ex.sort();
    let mut expected = vec![
        captured(&h_new, "/hour"),
        captured("checkout-b-old", "/old"),
        captured("checkout-a-rare", "/rare"),
    ];
    expected.sort();
    assert_eq!(ex, expected);

    // Redémarrage : les exemples sont relus, le snapshot les sert.
    c.shutdown().await;
    let c = start(config()).await.unwrap();
    let snap = mock_snapshot(&c, dir.path(), "checkout").await;
    for e in &expected {
        assert!(snap.contains(e), "{e:?} dans le snapshot : {snap:?}");
    }
    assert!(
        request_ids(&c, "checkout", "outbound")
            .await
            .contains(&"checkout-a-rare".to_string())
    );
}

#[tokio::test]
async fn ctl_17_examples_survive_memory_eviction_and_volume_cap() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let c = start(ControllerConfig {
        max_exchanges_per_workload: 3,
        max_exchanges_total: 4,
        max_bytes_per_workload: 2_000,
        ..config(dir.path())
    })
    .await
    .unwrap();
    // Workloads déjà vus : leurs échanges sont en mémoire dès l'ingestion.
    assert!(request_ids(&c, "checkout", "outbound").await.is_empty());
    assert!(request_ids(&c, "web", "outbound").await.is_empty());
    let out = |id: &str, path: &str, ms: u64| at(exchange(id, "outbound", path, 200, true), ms);
    push(
        &c,
        "checkout-a",
        &[out("checkout-a-rare", "/rare", now - 60_000)],
    )
    .await;
    for i in 0..8 {
        let id = format!("checkout-a-hot{i}");
        push(
            &c,
            "checkout-a",
            &[out(&id, &format!("/hot/{i}"), now - 50_000 + i)],
        )
        .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for i in 0..3 {
        let id = format!("web-a-{i}");
        push(&c, "web-a", &[out(&id, "/w", now - 40_000 + i)]).await;
    }
    // Évincé de la mémoire, l'échange reste listé grâce à son exemple.
    assert!(c.state().exchange_count("shop", "checkout") <= 3);
    let ids = request_ids(&c, "checkout", "outbound").await;
    assert!(ids.contains(&"checkout-a-rare".to_string()), "{ids:?}");

    // Plafond de volume : les plus anciens segments partent, l'exemple est écrit avant.
    c.state().maintain().await;
    let files = exchange_files(dir.path(), "checkout");
    let volume: u64 = files
        .iter()
        .map(|f| {
            std::fs::metadata(dir.path().join("shop/checkout/exchanges").join(f))
                .unwrap()
                .len()
        })
        .sum();
    assert!(volume <= 2_000, "{volume} octets");
    assert!(files.len() < 9, "segments anciens supprimés : {files:?}");
    let ex = exemplars(dir.path(), "checkout");
    assert!(ex.contains(&captured("checkout-a-rare", "/rare")), "{ex:?}");
    // Un seul exemple pour les chemins de même forme : le plus récent.
    let hot: Vec<_> = ex
        .iter()
        .filter(|(_, p, _)| p.starts_with("/hot/"))
        .collect();
    assert_eq!(hot.len(), 1, "{ex:?}");
    assert_eq!(hot[0].0, "checkout-a-hot7");

    let snap = mock_snapshot(&c, dir.path(), "checkout").await;
    assert!(
        snap.contains(&captured("checkout-a-rare", "/rare")),
        "{snap:?}"
    );
}

#[tokio::test]
async fn ctl_18_examples_are_bounded_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(ControllerConfig {
        max_exemplars_per_workload: 2,
        ..config(dir.path())
    })
    .await
    .unwrap();
    let out = |id: &str, path: &str, ms: u64| at(exchange(id, "outbound", path, 200, true), ms);
    push(&c, "checkout-a", &[out("checkout-a-1", "/a", 10)]).await;
    push(&c, "checkout-a", &[out("checkout-a-2", "/b", 20)]).await;
    push(&c, "checkout-a", &[out("checkout-a-3", "/c", 30)]).await;
    push(&c, "checkout-a", &[out("checkout-a-4", "/a", 40)]).await;
    let m = get_text(&c, "/metrics").await;
    assert_eq!(metric(&m, "mimic_controller_exemplars"), Some(2.0), "{m}");
    assert_eq!(
        metric(
            &m,
            r#"mimic_controller_exemplars_refused_total{reason="count"}"#
        ),
        Some(1.0),
        "{m}"
    );
    c.state().maintain().await;
    let ids: Vec<String> = exemplars(dir.path(), "checkout")
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert_eq!(ids, ["checkout-a-4", "checkout-a-2"]);
}

#[tokio::test]
async fn ctl_19_examples_never_mix_workloads() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(config(dir.path())).await.unwrap();
    let out = |id: &str, path: &str| exchange(id, "outbound", path, 200, true);
    push(&c, "checkout-a", &[out("checkout-a-1", "/checkout-only")]).await;
    push(&c, "web-a", &[out("web-a-1", "/web-only")]).await;
    // Un pod de web ne publie pas pour checkout (SEC-03).
    let r = client()
        .post(url(
            &c,
            "/v1/workloads/shop/checkout/segments?instance=web-a",
        ))
        .bearer_auth("tok-web-a")
        .body(jsonl(&[out("web-a-2", "/intrus")]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    c.state().maintain().await;
    let paths = |list: Vec<(String, String, String)>| -> Vec<String> {
        list.into_iter().map(|(_, p, _)| p).collect()
    };
    assert_eq!(paths(exemplars(dir.path(), "checkout")), ["/checkout-only"]);
    assert_eq!(paths(exemplars(dir.path(), "web")), ["/web-only"]);
    assert_eq!(
        paths(mock_snapshot(&c, dir.path(), "checkout").await),
        ["/checkout-only"]
    );
    assert_eq!(
        paths(mock_snapshot(&c, dir.path(), "web").await),
        ["/web-only"]
    );
}

/// Échange sortant dont le body de réponse fait `4 * n` caractères base64.
fn big(id: &str, path: &str, ms: u64, n: usize) -> Value {
    let mut e = at(exchange(id, "outbound", path, 200, true), ms);
    e["resp_body"]["data"] = json!("QUFB".repeat(n));
    e
}

fn serialized(v: &Value) -> u64 {
    let e: mimic_core::model::Exchange = serde_json::from_value(v.clone()).unwrap();
    mimic_core::exemplar::serialized_len(&e)
}

#[tokio::test]
async fn ctl_22_examples_are_bounded_in_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let a1 = big("checkout-a-1", "/a", 10, 500);
    let max = serialized(&a1) + 200;
    let c = start(ControllerConfig {
        max_exemplar_bytes_per_workload: max,
        max_exchanges_per_workload: 1,
        ..config(dir.path())
    })
    .await
    .unwrap();
    // Workload déjà vu : ses échanges sont en mémoire dès l'ingestion.
    assert!(request_ids(&c, "checkout", "outbound").await.is_empty());
    push(&c, "checkout-a", &[a1]).await;
    push(&c, "checkout-a", &[big("checkout-a-2", "/b", 20, 500)]).await;
    push(&c, "checkout-a", &[big("checkout-a-3", "/a", 30, 2_000)]).await;
    let m = get_text(&c, "/metrics").await;
    assert_eq!(
        metric(
            &m,
            r#"mimic_controller_exemplars_refused_total{reason="bytes"}"#
        ),
        Some(2.0),
        "{m}"
    );
    assert_eq!(metric(&m, "mimic_controller_exemplars"), Some(2.0), "{m}");
    push(&c, "checkout-a", &[big("checkout-a-4", "/a", 40, 1)]).await;
    // `/b` n'est plus en mémoire : son exemple sans body le garde dans la découverte.
    assert!(operation_paths(&c).await.contains(&"/b".to_string()));
    c.state().maintain().await;
    let mut ex = exemplars(dir.path(), "checkout");
    ex.sort();
    assert_eq!(
        ex,
        [
            ("checkout-a-2".into(), "/b".into(), "skipped".into()),
            captured("checkout-a-4", "/a"),
        ]
    );
    let m = get_text(&c, "/metrics").await;
    let kept = metric(&m, "mimic_controller_exemplars_bytes").unwrap();
    assert!(kept <= max as f64, "{kept} > {max}");
}

#[tokio::test]
async fn ctl_22_total_bound_spans_workloads() {
    let dir = tempfile::tempdir().unwrap();
    let a1 = big("checkout-a-1", "/a", 10, 500);
    let c = start(ControllerConfig {
        max_exemplar_bytes_total: serialized(&a1) + 200,
        ..config(dir.path())
    })
    .await
    .unwrap();
    push(&c, "checkout-a", &[a1]).await;
    push(&c, "web-a", &[big("web-a-1", "/w", 20, 500)]).await;
    let m = get_text(&c, "/metrics").await;
    assert_eq!(
        metric(
            &m,
            r#"mimic_controller_exemplars_refused_total{reason="bytes"}"#
        ),
        Some(1.0),
        "{m}"
    );
    c.state().maintain().await;
    assert_eq!(
        exemplars(dir.path(), "checkout"),
        [captured("checkout-a-1", "/a")]
    );
    assert_eq!(
        exemplars(dir.path(), "web"),
        [(
            "web-a-1".to_string(),
            "/w".to_string(),
            "skipped".to_string()
        )]
    );
}

/// Un exemple gardé sans body, relu de `exemplars.jsonl` après un redémarrage, est repris tel
/// quel : il ne prend pas la place d'un exemple complet, n'est pas compté une seconde fois
/// dans les bornes, reste non rejouable, puis redevient rejouable avec un échange plus récent
/// qui tient dans la borne.
#[tokio::test]
async fn ctl_22_example_without_body_is_restored_as_is_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    // `/z` tient dans la borne ; `/a`, ingéré ensuite, non : il est gardé sans body et le
    // précède dans `exemplars.jsonl` (ordre des endpoints). Ses headers, gardés sans body,
    // pèsent plus que la place laissée par `/z`. Un `/a` plus récent et léger (`a2`) tient
    // à côté de `/z`.
    let z = big("checkout-a-z1", "/z", 10, 500);
    let mut a1 = big("checkout-a-a1", "/a", 20, 500);
    a1["req_headers"] = json!([{"name": "x-trace", "value": "t".repeat(1_000)}]);
    let a2 = big("checkout-a-a2", "/a", 30, 1);
    let max = serialized(&z) + serialized(&a2) + 50;
    let mut bare = a1.clone();
    bare["resp_body"] = json!({"kind": "skipped", "size": 2000});
    assert!(serialized(&bare) > serialized(&a2) + 50);
    let config = || ControllerConfig {
        max_exemplar_bytes_per_workload: max,
        ..config(dir.path())
    };
    let c = start(config()).await.unwrap();
    push(&c, "checkout-a", &[z]).await;
    push(&c, "checkout-a", &[a1]).await;
    c.state().maintain().await;
    let skipped = (
        "checkout-a-a1".to_string(),
        "/a".to_string(),
        "skipped".to_string(),
    );
    let expected = vec![skipped, captured("checkout-a-z1", "/z")];
    assert_eq!(exemplars(dir.path(), "checkout"), expected);
    let m = get_text(&c, "/metrics").await;
    let kept = metric(&m, "mimic_controller_exemplars_bytes").unwrap();
    c.shutdown().await;

    // Segments sortis de la rétention : seuls restent les exemples.
    std::fs::remove_dir_all(dir.path().join("shop/checkout/exchanges")).unwrap();
    let file = dir.path().join("shop/checkout/exemplars.jsonl");
    let written = std::fs::read(&file).unwrap();
    let c = start(config()).await.unwrap();
    // Relus au premier besoin : l'endpoint sans body reste dans la découverte.
    assert_eq!(operation_paths(&c).await, ["/a", "/z"]);
    let m = get_text(&c, "/metrics").await;
    assert_eq!(metric(&m, "mimic_controller_exemplars"), Some(2.0), "{m}");
    assert_eq!(
        metric(&m, "mimic_controller_exemplars_bytes"),
        Some(kept),
        "{m}"
    );
    let refused = |m: &str| {
        metric(
            m,
            r#"mimic_controller_exemplars_refused_total{reason="bytes"}"#,
        )
        .unwrap_or(0.0)
    };
    assert_eq!(refused(&m), 0.0, "{m}");
    c.state().maintain().await;
    assert_eq!(std::fs::read(&file).unwrap(), written, "repris tel quel");
    // Non rejouable : le snapshot le porte sans body, `/z` garde le sien.
    assert_eq!(mock_snapshot(&c, dir.path(), "checkout").await, expected);

    // Un échange plus récent de `/a` qui tient dans la borne le rend rejouable.
    push(&c, "checkout-a", &[a2]).await;
    c.state().maintain().await;
    let replayable = vec![
        captured("checkout-a-a2", "/a"),
        captured("checkout-a-z1", "/z"),
    ];
    assert_eq!(exemplars(dir.path(), "checkout"), replayable);
    let m = get_text(&c, "/metrics").await;
    assert_eq!(refused(&m), 0.0, "{m}");
    let kept = metric(&m, "mimic_controller_exemplars_bytes").unwrap();
    assert!(kept <= max as f64, "{kept} > {max}");
    c.apply_spec(
        "shop",
        "checkout",
        Some(MimicWorkloadSpec {
            mode: "record".into(),
            snapshot: None,
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    assert_eq!(mock_snapshot(&c, dir.path(), "checkout").await, replayable);
}
