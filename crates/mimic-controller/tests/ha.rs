//! Critères CTL-25 à CTL-27 de la spec (§5.5.4) : contrôleur en deux réplicas, une seule
//! active, élue par un bail (ici en mémoire) sur un support partagé.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use mimic_controller::config::{AuthConfig, ControllerConfig, Identity, LeaderElection};
use mimic_controller::leader::{Coordination, MemoryLease};
use mimic_controller::reconcile::MimicWorkloadSpec;
use mimic_controller::{Running, start_elected};
use mimic_core::lease::{Lease, Record};
use serde_json::{Value, json};

const WL: &str = "/v1/workloads/shop/checkout";

/// Délais courts : bail de 1,2 s, abandon à 0,8 s, relecture toutes les 0,2 s.
fn election(identity: &str) -> LeaderElection {
    LeaderElection {
        identity: Some(identity.into()),
        lease_duration: Duration::from_millis(1_200),
        renew_deadline: Duration::from_millis(800),
        retry_period: Duration::from_millis(200),
        ..LeaderElection::default()
    }
}

fn config(dir: &std::path::Path, identity: &str) -> ControllerConfig {
    let tokens = BTreeMap::from([(
        "tok-a".to_string(),
        Identity {
            namespace: "shop".into(),
            pod: "checkout-a".into(),
            workload: "checkout".into(),
            ..Identity::default()
        },
    )]);
    ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ui_listen: "127.0.0.1:0".parse().unwrap(),
        store: format!("file://{}", dir.display()),
        auth: AuthConfig::Static { tokens },
        leader_election: Some(election(identity)),
        ..ControllerConfig::default()
    }
}

fn local(dir: &std::path::Path) -> Arc<dyn object_store::ObjectStore> {
    Arc::new(object_store::local::LocalFileSystem::new_with_prefix(dir).unwrap())
}

/// Bail partagé, vu par une réplique : ses écritures peuvent échouer, et la pose de sa
/// marque peut couper son support.
struct Replica {
    lease: Arc<MemoryLease>,
    fail_writes: Arc<AtomicBool>,
    on_mark: Option<Arc<AtomicBool>>,
}

impl Replica {
    fn new(lease: &Arc<MemoryLease>) -> Arc<Self> {
        Arc::new(Self {
            lease: lease.clone(),
            fail_writes: Arc::new(AtomicBool::new(false)),
            on_mark: None,
        })
    }
}

impl Coordination for Replica {
    fn get(&self) -> BoxFuture<'_, Result<Option<Lease>, String>> {
        self.lease.get()
    }

    fn write(&self, record: Record, expected: Option<String>) -> BoxFuture<'_, Result<(), String>> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Box::pin(async { Err("serveur injoignable".to_string()) });
        }
        self.lease.write(record, expected)
    }

    fn announce(&self, me: &str, active: bool) -> BoxFuture<'_, Result<(), String>> {
        if active && let Some(cut) = &self.on_mark {
            cut.store(true, Ordering::SeqCst);
        }
        self.lease.announce(me, active)
    }
}

/// Support qui peut échouer à toute opération ; compte les opérations refusées.
#[derive(Debug)]
struct Switch {
    inner: Arc<dyn object_store::ObjectStore>,
    fail: Arc<AtomicBool>,
    refused: Arc<AtomicUsize>,
}

impl std::fmt::Display for Switch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Switch({})", self.inner)
    }
}

impl Switch {
    fn check(&self) -> object_store::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(object_store::Error::Generic {
                store: "switch",
                source: "support coupé".into(),
            });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for Switch {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.check()?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.check()?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.check()?;
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
        if let Err(e) = self.check() {
            return Box::pin(futures_util::stream::once(async { Err(e) }));
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.check()?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.check()?;
        self.inner.copy_opts(from, to, options).await
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

fn exchange(id: &str, path: &str) -> Value {
    json!({
        "id": id, "direction": "outbound", "started_at_ms": 1_000, "scheme": "https",
        "authority": "api.saas.io:443", "method": "GET", "path": path, "query": [],
        "req_headers": [], "req_body": {"kind": "empty"},
        "status": 200, "resp_headers": [],
        "resp_body": {"kind": "captured", "data": "eyJpZCI6N30="},
        "ttfb_us": 1000, "total_us": 2000, "trace_id": null
    })
}

async fn post_segment(c: &Running, segment: &str, exchanges: &[Value]) -> reqwest::Response {
    let body: String = exchanges.iter().map(|e| format!("{e}\n")).collect();
    client()
        .post(url(
            c,
            &format!("{WL}/segments?instance=checkout-a&segment={segment}"),
        ))
        .bearer_auth("tok-a")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn heartbeat(c: &Running, mode: &str, snapshot: Option<&str>) -> reqwest::Response {
    client()
        .post(url(c, &format!("{WL}/heartbeat")))
        .bearer_auth("tok-a")
        .json(&json!({"instance": "checkout-a", "mode": mode, "snapshot": snapshot}))
        .send()
        .await
        .unwrap()
}

async fn mock_form(c: &Running, id: &str) -> (u16, Option<String>, Vec<u8>) {
    let r = client()
        .get(url(c, &format!("{WL}/snapshots/{id}/mock")))
        .bearer_auth("tok-a")
        .header("accept-encoding", "zstd")
        .send()
        .await
        .unwrap();
    let etag = r
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_string());
    (r.status().as_u16(), etag, r.bytes().await.unwrap().to_vec())
}

async fn metrics(c: &Running) -> String {
    client()
        .get(ui(c, "/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

fn metric(m: &str, name: &str) -> Option<f64> {
    m.lines()
        .find(|l| l.split_whitespace().next() == Some(name))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// Attend la marque de `c`, au plus `within`.
async fn leads_within(c: &Running, within: Duration) -> Duration {
    let t = Instant::now();
    tokio::time::timeout(within, c.leading())
        .await
        .unwrap_or_else(|_| panic!("pas active en {within:?}"));
    t.elapsed()
}

#[tokio::test]
async fn ctl_25_one_active_replica_and_handover() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Arc::new(MemoryLease::default());
    let a = start_elected(
        config(dir.path(), "ctl-a"),
        local(dir.path()),
        Replica::new(&lease),
    )
    .await
    .unwrap();
    let b = start_elected(
        config(dir.path(), "ctl-b"),
        local(dir.path()),
        Replica::new(&lease),
    )
    .await
    .unwrap();

    // Démarrage simultané : une seule active, et le reste.
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            _ = a.leading() => {}
            _ = b.leading() => {}
        }
    })
    .await
    .expect("une réplique active");
    for _ in 0..50 {
        assert!(
            a.is_leader() != b.is_leader(),
            "une seule active : a {}, b {}",
            a.is_leader(),
            b.is_leader()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (active, passive) = if a.is_leader() { (a, b) } else { (b, a) };
    assert_eq!(
        lease.marked().into_iter().collect::<Vec<_>>(),
        [lease.holder().unwrap()]
    );
    assert_eq!(heartbeat(&active, "record", None).await.status(), 200);
    assert_eq!(heartbeat(&passive, "record", None).await.status(), 503);

    // Arrêt propre : bail rendu, la passive le prend à sa relecture suivante.
    active.shutdown().await;
    let took = leads_within(&passive, Duration::from_secs(1)).await;
    assert!(took < Duration::from_millis(400), "relais en {took:?}");
    assert_eq!(heartbeat(&passive, "record", None).await.status(), 200);
    let b = passive;

    // Arrêt brutal de la nouvelle active : la troisième attend l'expiration du bail.
    let c = start_elected(
        config(dir.path(), "ctl-c"),
        local(dir.path()),
        Replica::new(&lease),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!c.is_leader());
    let killed = Instant::now();
    b.kill();
    tokio::time::timeout(Duration::from_secs(4), c.leading())
        .await
        .expect("c prend le bail expiré");
    let waited = killed.elapsed();
    assert!(
        waited >= Duration::from_millis(1_000),
        "pas avant l'expiration du bail : {waited:?}"
    );
    assert_eq!(lease.holder().as_deref(), Some("ctl-c"));

    // Active dont les écritures du bail échouent : elle cesse de servir et signale la perte
    // avant qu'une autre ne prenne le bail, sans écrire ses exemples.
    c.shutdown().await;
    let faulty = Replica::new(&lease);
    let d = start_elected(
        config(dir.path(), "ctl-d"),
        local(dir.path()),
        faulty.clone(),
    )
    .await
    .unwrap();
    leads_within(&d, Duration::from_secs(1)).await;
    assert_eq!(
        post_segment(&d, "01SEG", &[exchange("checkout-a-1", "/rare")])
            .await
            .status(),
        201
    );
    let e = start_elected(
        config(dir.path(), "ctl-e"),
        local(dir.path()),
        Replica::new(&lease),
    )
    .await
    .unwrap();
    faulty.fail_writes.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(2), d.lost())
        .await
        .expect("perte du bail signalée");
    let lost = Instant::now();
    assert!(!d.is_leader());
    assert!(!e.is_leader(), "e n'a pas pris le bail avant la perte de d");
    assert_eq!(heartbeat(&d, "record", None).await.status(), 503);
    tokio::time::timeout(Duration::from_secs(3), e.leading())
        .await
        .expect("e prend le relais");
    assert!(lost.elapsed() > Duration::from_millis(100));
    assert!(
        !dir.path().join("shop/checkout/exemplars.jsonl").exists(),
        "une active qui perd le bail n'écrit pas ses exemples"
    );
    d.kill();
    e.shutdown().await;
}

#[tokio::test]
async fn ctl_26_passive_replica_never_touches_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Arc::new(MemoryLease::default());
    // Bail tenu par une autre réplique, sur un bail long.
    lease
        .write(
            Record {
                holder: Some("ailleurs".into()),
                duration_ms: 60_000,
                transitions: 0,
            },
            None,
        )
        .await
        .unwrap();
    let fail = Arc::new(AtomicBool::new(true));
    let refused = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Switch {
        inner: local(dir.path()),
        fail: fail.clone(),
        refused: refused.clone(),
    });
    let p = start_elected(config(dir.path(), "ctl-p"), store, Replica::new(&lease))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!p.is_leader());

    let r = client().get(url(&p, "/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let passive = |r: reqwest::Response| {
        assert_eq!(r.status(), 503, "{}", r.url());
        assert_eq!(
            r.headers().get("retry-after").map(|v| v.to_str().unwrap()),
            Some("2")
        );
    };
    passive(heartbeat(&p, "record", None).await);
    passive(post_segment(&p, "01SEG", &[exchange("checkout-a-1", "/x")]).await);
    passive(
        client()
            .get(url(&p, &format!("{WL}/snapshots/0123456789abcdef/mock")))
            .bearer_auth("tok-a")
            .send()
            .await
            .unwrap(),
    );
    passive(client().get(ui(&p, "/v1/workloads")).send().await.unwrap());
    passive(client().get(ui(&p, "/")).send().await.unwrap());
    let m = metrics(&p).await;
    assert_eq!(metric(&m, "mimic_controller_leader"), Some(0.0), "{m}");
    assert_eq!(
        refused.load(Ordering::SeqCst),
        0,
        "aucune opération sur le support"
    );

    // Bail rendu : la passive devient active.
    let held = lease.get().await.unwrap().unwrap();
    lease
        .write(
            Record {
                holder: None,
                duration_ms: 1_000,
                transitions: 0,
            },
            Some(held.version),
        )
        .await
        .unwrap();
    fail.store(false, Ordering::SeqCst);
    leads_within(&p, Duration::from_secs(2)).await;
    let m = metrics(&p).await;
    assert_eq!(metric(&m, "mimic_controller_leader"), Some(1.0), "{m}");
    assert!(
        metric(&m, "mimic_controller_takeover_seconds").is_some_and(|s| s > 0.0),
        "{m}"
    );
    assert_eq!(heartbeat(&p, "record", None).await.status(), 200);
    p.shutdown().await;
}

#[tokio::test]
async fn ctl_27_takeover_reloads_the_persisted_state_before_marking() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Arc::new(MemoryLease::default());
    let a = start_elected(
        config(dir.path(), "ctl-a"),
        local(dir.path()),
        Replica::new(&lease),
    )
    .await
    .unwrap();
    leads_within(&a, Duration::from_secs(2)).await;
    let segment = [
        exchange("checkout-a-1", "/users/1"),
        exchange("checkout-a-2", "/rare"),
    ];
    assert_eq!(post_segment(&a, "01SEG", &segment).await.status(), 201);
    a.apply_spec(
        "shop",
        "checkout",
        Some(MimicWorkloadSpec {
            mode: "mock".into(),
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    let desired: Value = heartbeat(&a, "record", None).await.json().await.unwrap();
    assert_eq!(desired["mode"], "mock");
    let id = desired["snapshot"].as_str().unwrap().to_string();
    let (status, etag, before) = mock_form(&a, &id).await;
    assert_eq!(status, 200);

    // B : passive ; son support est coupé dès qu'elle pose sa marque.
    let cut = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Switch {
        inner: local(dir.path()),
        fail: cut.clone(),
        refused: refused.clone(),
    });
    let replica = Arc::new(Replica {
        lease: lease.clone(),
        fail_writes: Arc::new(AtomicBool::new(false)),
        on_mark: Some(cut.clone()),
    });
    let b = start_elected(config(dir.path(), "ctl-b"), store, replica)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!b.is_leader());

    // Arrêt propre de A : exemples écrits, puis bail rendu.
    a.shutdown().await;
    let written = std::fs::read_to_string(dir.path().join("shop/checkout/exemplars.jsonl"))
        .expect("A a écrit ses exemples");
    assert!(written.contains("/rare"), "{written}");
    leads_within(&b, Duration::from_secs(2)).await;
    assert!(cut.load(Ordering::SeqCst));

    // Support coupé : ce qui sert les sidecars a été rechargé avant la marque.
    let desired: Value = heartbeat(&b, "mock", Some(&id)).await.json().await.unwrap();
    assert_eq!(desired, json!({"mode": "mock", "snapshot": id}));
    let r = post_segment(&b, "01SEG", &segment).await;
    assert_eq!(r.status(), 200, "segment déjà reçu par A");
    let m = metrics(&b).await;
    assert_eq!(metric(&m, "mimic_controller_exemplars"), Some(2.0), "{m}");
    assert_eq!(
        metric(&m, r#"mimic_controller_segments_total{result="duplicate"}"#),
        Some(1.0),
        "{m}"
    );

    // Support rétabli : même forme de mock, mêmes octets, même ETag.
    cut.store(false, Ordering::SeqCst);
    let (status, etag_b, after) = mock_form(&b, &id).await;
    assert_eq!(status, 200);
    assert_eq!(etag_b, etag);
    assert_eq!(after, before);
    b.shutdown().await;
}
