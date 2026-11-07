//! CTL-18, status reconciler. The tests need a Valkey >= 9.0 whose URL is in
//! `MIMIC_TEST_VALKEY_URL` (for example `redis://127.0.0.1:6379`, no password).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report, Via};
use mimic_core::discovery::scope::ScopeKey;
use mimic_runtime::metrics::Metrics;
use serde_json::{Value, json};

use super::*;
use crate::store::{Push, StoreConfig, StoreMetrics};

const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    max_authorities: 64,
    sample_budget: 1 << 20,
};
const WINDOW: Duration = Duration::from_secs(300);

#[derive(Default)]
struct FakeSink {
    calls: Mutex<Vec<(String, String, Value)>>,
    fail: AtomicBool,
    hang: AtomicBool,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

/// Decrements the in-flight count when the apply ends or is abandoned.
struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeSink {
    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl StatusSink for FakeSink {
    fn apply<'a>(&'a self, ns: &'a str, name: &'a str, discovery: &'a Value) -> ApplyFuture<'a> {
        Box::pin(async move {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let _in_flight = InFlight(&self.in_flight);
            self.calls
                .lock()
                .unwrap()
                .push((ns.into(), name.into(), discovery.clone()));
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                Err("refused".into())
            } else {
                Ok(())
            }
        })
    }
}

fn valkey_url() -> String {
    std::env::var("MIMIC_TEST_VALKEY_URL").unwrap_or_else(|_| {
        panic!(
            "MIMIC_TEST_VALKEY_URL is not set: start a Valkey >= 9.0 (image of \
             `scripts/pinned-image.sh valkey`) and set it, e.g. redis://127.0.0.1:6379"
        )
    })
}

fn unique(prefix: &str) -> String {
    let mut bytes = [0u8; 6];
    rand::fill(&mut bytes[..]);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

struct Setup {
    reconciler: Reconciler,
    sink: Arc<FakeSink>,
    metrics: Metrics,
}

fn setup(url: &str, workloads: &[(&str, &str, &str)]) -> Setup {
    let mut metrics = Metrics::new("controller", "test");
    let store_metrics = StoreMetrics::register(metrics.registry_mut());
    let status_metrics = StatusMetrics::register(metrics.registry_mut());
    let config = StoreConfig {
        ttl: Duration::from_secs(3600),
        limits: LIMITS,
        max_deployments: 50,
    };
    let client = redis::Client::open(url).expect("valid URL");
    let sink = Arc::new(FakeSink::default());
    let reconciler = Reconciler {
        store: Store::new(client, config, store_metrics),
        cache: Arc::new(WorkloadCache::synced_with(workloads)),
        sink: sink.clone(),
        metrics: status_metrics,
        window: WINDOW,
        apply_timeout: Duration::from_millis(200),
        down: AtomicBool::new(false),
    };
    Setup {
        reconciler,
        sink,
        metrics,
    }
}

async fn push(store: &Arc<Store>, ns: &str, dep: &str, pod: &str, paths: &[&str]) {
    let mut report = Report::new(Some(pod.to_string()));
    for path in paths {
        report.record(
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "GET",
                path,
                status: 200,
                head_ms: 3,
                host: Some("svc"),
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    report.stamp(now_ms() / 86_400_000);
    let push = Push {
        pod: pod.to_string(),
        seq: None,
        json: report.to_json().expect("serialises"),
    };
    store.push(ns, dep, push).await.expect("push applied");
}

/// One inbound and one outbound exchange (authority `api.example.com:443`).
async fn push_both(store: &Arc<Store>, ns: &str, dep: &str, pod: &str) {
    let mut report = Report::new(Some(pod.to_string()));
    for (key, direction, path) in [
        (ScopeKey::Port(80), Direction::Inbound, "/a"),
        (
            ScopeKey::Authority("api.example.com:443".to_string()),
            Direction::Outbound,
            "/v1",
        ),
    ] {
        report.record_at(
            key,
            &Via {
                tls: false,
                sni: None,
                dst: None,
            },
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction,
                port: 80,
                method: "GET",
                path,
                status: 200,
                head_ms: 3,
                host: Some("svc"),
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
    report.stamp(now_ms() / 86_400_000);
    let push = Push {
        pod: pod.to_string(),
        seq: None,
        json: report.to_json().expect("serialises"),
    };
    store.push(ns, dep, push).await.expect("push applied");
}

fn count(metrics: &Metrics, result: &str) -> f64 {
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with("mimic_status_updates_total") && l.contains(result))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

#[tokio::test]
async fn ctl_18_every_workload_naming_the_deployment_gets_the_status() {
    let ns = unique("ctl18");
    let s = setup(
        &valkey_url(),
        &[
            (&ns, "w1", "users"),
            (&ns, "w2", "users"),
            (&ns, "w3", "front"),
        ],
    );
    push(
        &s.reconciler.store,
        &ns,
        "users",
        "users-a",
        &["/a", "/b/1"],
    )
    .await;
    push(&s.reconciler.store, &ns, "users", "users-b", &["/a"]).await;
    s.reconciler.tick(now_ms()).await;
    let calls = s.sink.calls();
    let by_name = |name: &str| calls.iter().find(|c| c.1 == name).map(|c| c.2.clone());
    let users = by_name("w1").expect("w1 applied");
    assert_eq!(by_name("w2"), Some(users.clone()));
    assert_eq!(users["endpoints"], json!({"inbound": 2}));
    assert_eq!(users["totalEndpoints"], json!(2));
    assert_eq!(users["pods"], json!(2));
    assert_eq!(users["exchanges"], json!(3));
    assert!(users["lastPushTime"].as_str().unwrap().ends_with('Z'));
    // A deployment without data gets the empty status.
    assert_eq!(
        by_name("w3"),
        Some(
            json!({"endpoints": {}, "totalEndpoints": 0, "exchanges": 0, "sampleBytes": 0, "pods": 0})
        )
    );
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|c| c.0 == ns));
    assert_eq!(count(&s.metrics, "ok"), 3.0);
}

#[tokio::test]
async fn ctl_18_an_unchanged_status_sends_no_request() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    push(&s.reconciler.store, &ns, "users", "users-a", &["/a"]).await;
    let now = now_ms();
    s.reconciler.tick(now).await;
    let first = s.sink.calls();
    assert_eq!(first.len(), 1);
    // The watch delivers the applied status back into the cache.
    s.reconciler
        .cache
        .set_status(&ns, "w1", Some(first[0].2.clone()));
    s.reconciler.tick(now + 1000).await;
    s.reconciler.tick(now + 2000).await;
    assert_eq!(s.sink.calls().len(), 1);
    // A change of the input (a new pod) is applied once.
    push(&s.reconciler.store, &ns, "users", "users-b", &["/a"]).await;
    s.reconciler.tick(now_ms()).await;
    let calls = s.sink.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].2["pods"], json!(2));
}

#[tokio::test]
async fn ctl_18_a_pod_leaves_the_status_after_the_window() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    push(&s.reconciler.store, &ns, "users", "users-a", &["/a"]).await;
    let now = now_ms();
    s.reconciler.tick(now).await;
    let later = now + WINDOW.as_millis() as u64 + 5000;
    s.reconciler.tick(later).await;
    let calls = s.sink.calls();
    assert_eq!(calls[0].2["pods"], json!(1));
    assert_eq!(calls[1].2["pods"], json!(0));
    assert_eq!(calls[1].2["endpoints"], json!({"inbound": 1}));
}

#[tokio::test]
async fn ctl_18_a_failed_apply_is_counted_and_retried_at_the_next_pass() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    s.sink.fail.store(true, Ordering::SeqCst);
    s.reconciler.tick(now_ms()).await;
    s.sink.fail.store(false, Ordering::SeqCst);
    s.reconciler.tick(now_ms()).await;
    assert_eq!(s.sink.calls().len(), 2);
    assert_eq!(count(&s.metrics, "error"), 1.0);
    assert_eq!(count(&s.metrics, "ok"), 1.0);
}

#[tokio::test]
async fn ctl_18_while_valkey_is_down_no_request_is_sent_and_one_warn_is_written() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let ns = unique("ctl18");
    let s = setup("redis://127.0.0.1:1", &[(&ns, "w1", "users")]);
    for _ in 0..3 {
        s.reconciler.tick(now_ms()).await;
    }
    assert!(s.sink.calls().is_empty());
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let warns = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["level"] == "WARN" && v["message"] == "status not updated")
        .count();
    assert_eq!(warns, 1, "{raw}");
    assert_eq!(count(&s.metrics, "error"), 0.0);
}

#[tokio::test]
async fn ctl_18_recovery_clears_the_outage_and_applies_the_status() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    s.reconciler.down.store(true, Ordering::SeqCst);
    s.reconciler.tick(now_ms()).await;
    assert!(!s.reconciler.down.load(Ordering::SeqCst));
    assert_eq!(s.sink.calls().len(), 1);
}

#[test]
fn ctl_18_the_cache_lists_workloads_with_the_status_they_hold() {
    let cache = WorkloadCache::synced_with(&[("shop", "w1", "api"), ("shop", "w2", "web")]);
    cache.set_status("shop", "w1", Some(json!({"pods": 1})));
    let targets = cache.targets();
    assert_eq!(targets.len(), 2);
    assert_eq!(
        (targets[0].name.as_str(), targets[0].deployment.as_str()),
        ("w1", "api")
    );
    assert_eq!(targets[0].status, Some(json!({"pods": 1})));
    assert_eq!(targets[1].status, None);
    cache.remove("shop", "w1");
    assert_eq!(cache.targets().len(), 1);
    cache.set_status("shop", "gone", Some(json!({})));
    assert_eq!(cache.targets().len(), 1);
}

#[test]
fn ctl_18_the_status_apply_is_forced_under_the_controller_manager() {
    let params = apply_params();
    assert!(params.force);
    assert_eq!(params.field_manager.as_deref(), Some("mimic-controller"));
}

#[tokio::test]
async fn ctl_18_an_apply_that_hangs_is_abandoned_counted_and_logged() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users"), (&ns, "w2", "users")]);
    s.sink.hang.store(true, Ordering::SeqCst);
    let pass = s.reconciler.tick(now_ms());
    tokio::time::timeout(Duration::from_secs(5), pass)
        .await
        .expect("the pass ends although the apply hangs");
    // Both workloads were tried: the hanging one did not hold the pass.
    assert_eq!(s.sink.calls().len(), 2);
    assert_eq!(count(&s.metrics, "error"), 2.0);
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(raw.contains("status apply failed"), "{raw}");
}

#[tokio::test]
async fn ctl_18_a_status_without_the_defaulted_inbound_count_is_unchanged() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "front")]);
    // The CRD defaults `endpoints.inbound` to 0, so the watch returns it.
    s.reconciler.cache.set_status(
        &ns,
        "w1",
        Some(json!({"endpoints": {"inbound": 0}, "totalEndpoints": 0, "exchanges": 0, "sampleBytes": 0, "pods": 0})),
    );
    s.reconciler.tick(now_ms()).await;
    assert!(s.sink.calls().is_empty());
}

#[tokio::test]
async fn shr_12_a_status_without_the_defaulted_total_is_unchanged() {
    let ns = unique("shr12");
    let s = setup(&valkey_url(), &[(&ns, "w1", "front")]);
    // Neither default is stored: the cache holds what the controller applied.
    s.reconciler.cache.set_status(
        &ns,
        "w1",
        Some(json!({"endpoints": {}, "exchanges": 0, "sampleBytes": 0, "pods": 0})),
    );
    s.reconciler.tick(now_ms()).await;
    assert!(s.sink.calls().is_empty());
    // A status of a controller up to F7 with data differs by the total.
    push(&s.reconciler.store, &ns, "front", "front-a", &["/a"]).await;
    s.reconciler.cache.set_status(
        &ns,
        "w1",
        Some(json!({"endpoints": {"inbound": 1}, "exchanges": 1, "sampleBytes": 0, "pods": 1})),
    );
    s.reconciler.tick(now_ms()).await;
    let calls = s.sink.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2["totalEndpoints"], json!(1));
}

#[tokio::test]
async fn shr_12_outbound_endpoints_and_the_total_are_applied() {
    let ns = unique("shr12");
    let s = setup(&valkey_url(), &[(&ns, "w1", "front")]);
    push_both(&s.reconciler.store, &ns, "front", "front-a").await;
    s.reconciler.tick(now_ms()).await;
    let calls = s.sink.calls();
    assert_eq!(calls.len(), 1);
    let status = &calls[0].2;
    assert_eq!(status["endpoints"], json!({"inbound": 1, "outbound": 1}));
    assert_eq!(status["totalEndpoints"], json!(2));
    // The exchanges currently held, both directions.
    assert_eq!(status["exchanges"], json!(2));
}

fn unreadable_errors(capture: &crate::webhook::tests::Capture, ns: &str) -> usize {
    String::from_utf8(capture.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["level"] == "ERROR" && v["namespace"] == ns)
        .filter(|v| {
            v["message"]
                .as_str()
                .is_some_and(|m| m.ends_with("unreadable"))
        })
        .count()
}

async fn corrupt(ns: &str, dep: &str, field: &str, value: &str, pods: bool) {
    let k = crate::store::keys(ns, dep).unwrap();
    let key = if pods { k.pods } else { k.meta };
    let mut conn = redis::Client::open(valkey_url().as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg(key)
        .arg(field)
        .arg(value)
        .query_async(&mut conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn ctl_18_an_unreadable_meta_is_logged_once_per_streak() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    push(&s.reconciler.store, &ns, "users", "users-a", &["/a"]).await;
    corrupt(&ns, "users", "exchanges", "many", false).await;
    for _ in 0..3 {
        s.reconciler.tick(now_ms()).await;
    }
    assert_eq!(unreadable_errors(&capture, &ns), 1);
    assert!(s.sink.calls().is_empty());
    // Readable again, then broken again: a new streak, a new line.
    corrupt(&ns, "users", "exchanges", "7", false).await;
    s.reconciler.tick(now_ms()).await;
    assert_eq!(s.sink.calls().len(), 1);
    corrupt(&ns, "users", "exchanges", "many", false).await;
    s.reconciler.tick(now_ms()).await;
    s.reconciler.tick(now_ms()).await;
    assert_eq!(unreadable_errors(&capture, &ns), 2);
}

#[tokio::test]
async fn ctl_18_unreadable_pods_are_logged_as_pods() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users")]);
    push(&s.reconciler.store, &ns, "users", "users-a", &["/a"]).await;
    corrupt(&ns, "users", "users-a", "yesterday", true).await;
    s.reconciler.tick(now_ms()).await;
    s.reconciler.tick(now_ms()).await;
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert_eq!(raw.matches("stored pods unreadable").count(), 1, "{raw}");
    assert_eq!(raw.matches("stored meta unreadable").count(), 0, "{raw}");
}

#[tokio::test]
async fn ctl_18_hung_applies_run_concurrently_within_a_fixed_bound() {
    let ns = unique("ctl18");
    let names: Vec<String> = (0..20).map(|i| format!("w{i:02}")).collect();
    let workloads: Vec<(&str, &str, &str)> = names
        .iter()
        .map(|n| (ns.as_str(), n.as_str(), "users"))
        .collect();
    let s = setup(&valkey_url(), &workloads);
    s.sink.hang.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    s.reconciler.tick(now_ms()).await;
    // 20 hung applies of 200 ms: 3 rounds of 8 at most, not 20 in a row (4 s). #293 note 25.
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(s.sink.calls().len(), 20);
    assert_eq!(s.sink.max_in_flight.load(Ordering::SeqCst), 8);
    assert_eq!(count(&s.metrics, "error"), 20.0);
}

#[tokio::test]
async fn ctl_18_concurrent_applies_still_skip_unchanged_statuses() {
    let ns = unique("ctl18");
    let s = setup(&valkey_url(), &[(&ns, "w1", "users"), (&ns, "w2", "users")]);
    s.reconciler.tick(now_ms()).await;
    let first = s.sink.calls();
    assert_eq!(first.len(), 2);
    for call in &first {
        s.reconciler
            .cache
            .set_status(&ns, &call.1, Some(call.2.clone()));
    }
    s.reconciler.tick(now_ms()).await;
    assert_eq!(s.sink.calls().len(), 2);
}
