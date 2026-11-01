//! CTL-05, CTL-09 and CTL-10. The integration tests need a Valkey >= 9.0 whose URL is in
//! `MIMIC_TEST_VALKEY_URL` (for example `redis://127.0.0.1:6379`, no password).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mimic_core::discovery::observed::Observation;
use mimic_core::discovery::redact::Redactor;
use mimic_core::discovery::report::{Direction, Exchange, Limits, Report};
use mimic_core::discovery::store::merge_bounded;
use mimic_runtime::metrics::Metrics;

use super::*;

const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    sample_budget: 1 << 20,
};
const SESSION: &str = "0123456789abcdef0123456789abcdef";
const DAY: u64 = 86_400;

fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / DAY)
}

fn valkey_url(db: Option<u8>) -> String {
    let url = std::env::var("MIMIC_TEST_VALKEY_URL").unwrap_or_else(|_| {
        panic!(
            "MIMIC_TEST_VALKEY_URL is not set: start a Valkey >= 9.0 (image of \
             `scripts/pinned-image.sh valkey`) and set it, e.g. redis://127.0.0.1:6379"
        )
    });
    match db {
        Some(db) => format!("{}/{db}", url.trim_end_matches('/')),
        None => url,
    }
}

fn client(db: Option<u8>) -> redis::Client {
    redis::Client::open(valkey_url(db)).expect("MIMIC_TEST_VALKEY_URL is not a valid URL")
}

fn config(ttl: Duration) -> StoreConfig {
    StoreConfig {
        ttl,
        limits: LIMITS,
        max_deployments: 50,
    }
}

fn replica(db: Option<u8>, config: StoreConfig) -> (Arc<Store>, Metrics) {
    let mut metrics = Metrics::new("controller", "test");
    let store_metrics = StoreMetrics::register(metrics.registry_mut());
    (Store::new(client(db), config, store_metrics), metrics)
}

fn unique(prefix: &str) -> String {
    let mut bytes = [0u8; 6];
    rand::fill(&mut bytes[..]);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

fn exchange(report: &mut Report, path: &str, status: u16) {
    report.record(
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: 80,
            method: "GET",
            path,
            status,
            head_ms: 3,
            host: Some("svc"),
            grpc: false,
            observation: Observation::default(),
        },
    );
}

fn push_of(pod: &str, seq: Option<u64>, paths: &[(&str, u16)]) -> Push {
    let mut report = Report::new(Some(pod.to_string()));
    for (path, status) in paths {
        exchange(&mut report, path, *status);
    }
    report.stamp(today());
    Push {
        pod: pod.to_string(),
        seq: seq.map(|n| SeqId::parse(SESSION, n).expect("valid seq")),
        report,
    }
}

async fn conn(db: Option<u8>) -> redis::aio::MultiplexedConnection {
    client(db)
        .get_multiplexed_async_connection()
        .await
        .expect("Valkey is reachable")
}

async fn cmd<T: redis::FromRedisValue>(
    conn: &mut redis::aio::MultiplexedConnection,
    args: &[&str],
) -> T {
    let mut c = redis::cmd(args[0]);
    for a in &args[1..] {
        c.arg(*a);
    }
    c.query_async(conn).await.expect("command succeeds")
}

fn counter(metrics: &Metrics, name: &str, label: &str) -> f64 {
    metrics
        .encode()
        .lines()
        .find(|l| l.contains(name) && l.contains(label))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

#[test]
fn ctl_05_key_names_are_byte_exact() {
    let k = keys("shop", "pay.v2").expect("valid names");
    assert_eq!(k.report, "mimic:v1:{shop/pay.v2}:report");
    assert_eq!(k.meta, "mimic:v1:{shop/pay.v2}:meta");
    assert_eq!(k.pods, "mimic:v1:{shop/pay.v2}:pods");
    assert_eq!(k.seq, "mimic:v1:{shop/pay.v2}:seq");
    assert_eq!(k.ns_index, "mimic:v1:ns:shop");
    assert_eq!(k.workloads, "mimic:v1:workloads");
}

#[test]
fn ctl_05_names_that_are_not_dns_1123_are_refused() {
    for bad in [
        "", "Shop", "a:b", "a/b", "a{b", "a}b", "-a", "a-", ".a", "a b", "a_b", "a\n",
    ] {
        assert!(keys(bad, "pay").is_err(), "namespace {bad:?}");
        assert!(keys("shop", bad).is_err(), "deployment {bad:?}");
    }
    assert!(keys("shop", &"a".repeat(254)).is_err());
    assert!(keys("shop", &"a".repeat(253)).is_ok());
}

#[test]
fn ctl_05_sequence_decision() {
    assert_eq!(decide(None, Some(1)), Decision::Apply);
    assert_eq!(decide(Some(4), Some(5)), Decision::Apply);
    assert_eq!(decide(Some(4), Some(4)), Decision::Duplicate);
    assert_eq!(decide(Some(4), Some(3)), Decision::Duplicate);
    assert_eq!(decide(Some(4), None), Decision::Apply);
    assert_eq!(decide(None, None), Decision::Apply);
}

#[test]
fn ctl_05_session_and_sequence_format() {
    assert!(SeqId::parse(SESSION, 1).is_ok());
    assert!(SeqId::parse(SESSION, 1 << 53).is_ok());
    assert!(SeqId::parse(SESSION, (1 << 53) + 1).is_err());
    assert!(SeqId::parse(SESSION, 0).is_err());
    assert!(SeqId::parse(SESSION, u64::MAX).is_err());
    for bad in [
        "",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcde/",
    ] {
        assert!(SeqId::parse(bad, 1).is_err(), "session {bad:?}");
    }
}

/// Small deterministic generator for the generated pushes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctl_09_applied_once_across_replicas() {
    const PATHS: [&str; 6] = ["/a", "/b", "/c/1", "/c/2", "/d", "/e/x"];
    let (a, metrics_a) = replica(None, config(Duration::from_secs(3600)));
    let (b, metrics_b) = replica(None, config(Duration::from_secs(3600)));
    let ns = unique("ctl09");
    let deployments = ["front".to_string(), "back".to_string()];
    let distinct: Arc<Mutex<[Vec<Report>; 2]>> = Arc::default();

    let mut tasks = Vec::new();
    for n in 0..16u64 {
        let (a, b) = (a.clone(), b.clone());
        let (ns, distinct) = (ns.clone(), distinct.clone());
        let dep_index = (n % 2) as usize;
        let dep = deployments[dep_index].clone();
        tasks.push(tokio::spawn(async move {
            let pod = format!("pod-{n}");
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (n + 1));
            for seq in 1..=50u64 {
                let k = 1 + (rng.next() % 3) as usize;
                let exchanges: Vec<(&str, u16)> = (0..k)
                    .map(|_| {
                        let r = rng.next();
                        (
                            PATHS[(r % 6) as usize],
                            if r.is_multiple_of(7) { 404 } else { 200 },
                        )
                    })
                    .collect();
                let make = || push_of(&pod, Some(seq), &exchanges);
                distinct.lock().expect("lock")[dep_index].push(make().report);
                let pick = |r: u64| {
                    if r.is_multiple_of(2) {
                        a.clone()
                    } else {
                        b.clone()
                    }
                };
                let roll = rng.next() % 100;
                if roll < 10 {
                    let (x, y) = tokio::join!(a.push(&ns, &dep, make()), b.push(&ns, &dep, make()));
                    let mut got = [x.expect("push"), y.expect("push")];
                    got.sort_by_key(|o| *o == Outcome::Duplicate);
                    assert_eq!(got, [Outcome::Applied, Outcome::Duplicate]);
                } else {
                    let first = pick(rng.next())
                        .push(&ns, &dep, make())
                        .await
                        .expect("push");
                    assert_eq!(first, Outcome::Applied);
                    if roll < 40 {
                        let again = pick(rng.next())
                            .push(&ns, &dep, make())
                            .await
                            .expect("push");
                        assert_eq!(again, Outcome::Duplicate);
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await.expect("sidecar task");
    }

    let mut c = conn(None).await;
    for (i, dep) in deployments.iter().enumerate() {
        let k = keys(&ns, dep).expect("keys");
        let stored: String = cmd(&mut c, &["GET", &k.report]).await;
        let pushes = distinct.lock().expect("lock")[i].clone();
        assert_eq!(pushes.len(), 8 * 50);
        let exchanges: u64 = pushes.iter().map(Report::exchanges).sum();
        let mut expected = Report::new(None);
        for p in pushes {
            expected = merge_bounded(expected, p, &LIMITS).report;
        }
        assert_eq!(stored, expected.to_json().expect("json"));
        let got: String = cmd(&mut c, &["HGET", &k.meta, "exchanges"]).await;
        assert_eq!(got, exchanges.to_string());
    }
    let conflicts = counter(&metrics_a, "store_writes_total", "conflict")
        + counter(&metrics_b, "store_writes_total", "conflict");
    assert!(conflicts > 0.0, "no contention happened");
}

#[tokio::test]
async fn ctl_09_unreadable_stored_value_is_never_overwritten() {
    let (store, _) = replica(None, config(Duration::from_secs(3600)));
    let (ns, dep) = (unique("ctl09u"), "front");
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let _: () = cmd(&mut c, &["SET", &k.report, "{not json"]).await;
    let got = store
        .push(&ns, dep, push_of("p", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(got, Err(StoreError::Unavailable));
    let stored: String = cmd(&mut c, &["GET", &k.report]).await;
    assert_eq!(stored, "{not json");
}

#[tokio::test]
async fn ctl_10_new_deployment_past_the_namespace_cap_is_refused() {
    let mut cfg = config(Duration::from_secs(3600));
    cfg.max_deployments = 2;
    let (store, _) = replica(None, cfg);
    let ns = unique("ctl10c");
    for dep in ["one", "two"] {
        let got = store
            .push(&ns, dep, push_of("p", Some(1), &[("/a", 200)]))
            .await;
        assert_eq!(got, Ok(Outcome::Applied));
    }
    let got = store
        .push(&ns, "three", push_of("p", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(got, Err(StoreError::Deployments));
    // An existing deployment keeps writing.
    let got = store
        .push(&ns, "one", push_of("p", Some(2), &[("/a", 200)]))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let k = keys(&ns, "three").expect("keys");
    let mut c = conn(None).await;
    let exists: i64 = cmd(&mut c, &["EXISTS", &k.report]).await;
    assert_eq!(exists, 0);
}

#[tokio::test]
async fn ctl_10_keys_types_and_field_expiry_with_the_default_ttl() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("ctl10k"), "pay");
    let got = store
        .push(
            &ns,
            dep,
            push_of("pod-a", Some(1), &[("/a", 200), ("/b", 500)]),
        )
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;

    let pattern = format!("mimic:v1:{{{ns}/{dep}}}:*");
    let mut found: Vec<String> = cmd(&mut c, &["KEYS", &pattern]).await;
    found.sort();
    let mut want = vec![
        k.meta.clone(),
        k.pods.clone(),
        k.report.clone(),
        k.seq.clone(),
    ];
    want.sort();
    assert_eq!(found, want);
    for (key, ty) in [
        (&k.report, "string"),
        (&k.meta, "hash"),
        (&k.pods, "hash"),
        (&k.seq, "hash"),
        (&k.ns_index, "zset"),
        (&k.workloads, "zset"),
    ] {
        let got: String = cmd(&mut c, &["TYPE", key]).await;
        assert_eq!(got, ty, "{key}");
    }
    let mut fields: Vec<String> = cmd(&mut c, &["HKEYS", &k.meta]).await;
    fields.sort();
    let mut want: Vec<String> = [
        "version",
        "updated",
        "exchanges",
        "endpoints_inbound",
        "endpoints_outbound",
        "sample_bytes",
        "report_bytes",
    ]
    .map(String::from)
    .to_vec();
    want.sort();
    assert_eq!(fields, want);
    let version: String = cmd(&mut c, &["HGET", &k.meta, "version"]).await;
    assert_eq!(version, "1");
    let exchanges: String = cmd(&mut c, &["HGET", &k.meta, "exchanges"]).await;
    assert_eq!(exchanges, "2");
    let inbound: String = cmd(&mut c, &["HGET", &k.meta, "endpoints_inbound"]).await;
    assert_eq!(inbound, "2");
    let seq: String = cmd(&mut c, &["HGET", &k.seq, &format!("pod-a/{SESSION}")]).await;
    assert_eq!(seq, "1");
    let members: Vec<String> = cmd(&mut c, &["ZRANGE", &k.ns_index, "0", "-1"]).await;
    assert_eq!(members, [dep]);
    let member = format!("{ns}/{dep}");
    let score: Option<String> = cmd(&mut c, &["ZSCORE", &k.workloads, &member]).await;
    assert!(score.is_some());

    let thirty_days = i64::try_from(30 * DAY * 1000).expect("fits");
    for key in [&k.report, &k.meta, &k.ns_index] {
        let ttl: i64 = cmd(&mut c, &["PTTL", key]).await;
        assert!(
            ttl > thirty_days - 60_000 && ttl <= thirty_days,
            "{key}: {ttl}"
        );
    }
    let day_ms = i64::try_from(DAY * 1000).expect("fits");
    for (key, field) in [
        (&k.pods, "pod-a".to_string()),
        (&k.seq, format!("pod-a/{SESSION}")),
    ] {
        let ttl: Vec<i64> = cmd(&mut c, &["HPTTL", key, "FIELDS", "1", &field]).await;
        assert!(
            ttl[0] > day_ms - 60_000 && ttl[0] <= day_ms,
            "{key}: {ttl:?}"
        );
    }
}

#[tokio::test]
async fn ctl_10_expiry_refreshed_by_applied_pushes_and_everything_expires() {
    // A database of its own: the index keys are shared by every deployment.
    let db = Some(15);
    let mut c = conn(db).await;
    let _: () = cmd(&mut c, &["FLUSHDB"]).await;
    let (store, _) = replica(db, config(Duration::from_secs(10)));
    let (ns, dep) = (unique("ctl10t"), "pay");
    let k = keys(&ns, dep).expect("keys");
    let reader = c.clone();
    let ttl_of = |key: String| {
        let mut c = reader.clone();
        async move { cmd::<i64>(&mut c, &["PTTL", &key]).await }
    };

    let first = store
        .push(&ns, dep, push_of("pod-a", Some(1), &[("/a", 200)]))
        .await;
    assert_eq!(first, Ok(Outcome::Applied));
    for key in [&k.report, &k.meta, &k.ns_index, &k.workloads] {
        let ttl = ttl_of(key.clone()).await;
        assert!(ttl > 0 && ttl <= 10_000, "{key}: {ttl}");
    }
    for (key, field) in [
        (&k.pods, "pod-a".to_string()),
        (&k.seq, format!("pod-a/{SESSION}")),
    ] {
        let ttl: Vec<i64> = cmd(&mut c, &["HPTTL", key, "FIELDS", "1", &field]).await;
        assert!(ttl[0] > 0 && ttl[0] <= 10_000, "{key}: {ttl:?}");
    }

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(ttl_of(k.report.clone()).await < 8_000);
    let second = store
        .push(&ns, dep, push_of("pod-a", Some(2), &[("/a", 200)]))
        .await;
    assert_eq!(second, Ok(Outcome::Applied));
    for key in [&k.report, &k.meta, &k.ns_index, &k.workloads] {
        assert!(ttl_of(key.clone()).await > 9_000, "{key} not refreshed");
    }

    tokio::time::sleep(Duration::from_secs(3)).await;
    let again = store
        .push(&ns, dep, push_of("pod-a", Some(2), &[("/a", 200)]))
        .await;
    assert_eq!(again, Ok(Outcome::Duplicate));
    assert!(
        ttl_of(k.report.clone()).await < 8_000,
        "a duplicate refreshed the report"
    );
    let pods: Vec<i64> = cmd(&mut c, &["HPTTL", &k.pods, "FIELDS", "1", "pod-a"]).await;
    assert!(pods[0] > 9_000, "a duplicate did not refresh :pods");

    tokio::time::sleep(Duration::from_secs(14)).await;
    let left: BTreeSet<String> = cmd::<Vec<String>>(&mut c, &["KEYS", "*"])
        .await
        .into_iter()
        .collect();
    assert!(left.is_empty(), "left after expiry: {left:?}");
}
