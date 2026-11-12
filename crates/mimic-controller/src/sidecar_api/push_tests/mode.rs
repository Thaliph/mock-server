//! F10 step 2: the mode on the egress route (spec §7.7) and `:pulls` (§7.3): MCK-08.

use super::egress::{Pulled, pull, push_report, request};
use super::v2::{AUTHORITY, outbound_report};
use super::*;
use crate::workloads::MockSettings;

const SESSION: &str = "0123456789abcdef0123456789abcdef";

/// Sets `spec.mode` and `spec.mock.latencyFactor` (hundredths) of a `MimicWorkload` naming `pay`.
fn workload(s: &Setup, namespace: &str, name: &str, mock: bool, factor: u32) {
    s.api
        .workloads
        .apply(namespace, name, "pay", vec![], vec![]);
    s.api.workloads.set_mock(
        namespace,
        name,
        MockSettings {
            mock,
            factor,
            passthrough: Vec::new(),
        },
    );
}

fn modes(answer: &Pulled) -> (Option<&str>, Option<&str>) {
    (
        answer.header("mimic-mode"),
        answer.header("mimic-latency-factor"),
    )
}

/// `:pulls` once `done` holds: the write runs after the answer.
async fn pulls_when(
    s: &Setup,
    deployment: &str,
    done: impl Fn(&BTreeMap<String, String>) -> bool,
) -> BTreeMap<String, String> {
    for _ in 0..200 {
        let found = pulls(s, deployment).await;
        if done(&found) {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("`:pulls` never reached the expected state");
}

async fn pulls(s: &Setup, deployment: &str) -> BTreeMap<String, String> {
    let key = keys(&s.ns, deployment).unwrap().pulls;
    redis::cmd("HGETALL")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn mck_08_200_and_304_carry_the_mode_outside_the_etag() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    workload(&s, &s.ns, "pay-workload", true, 50);

    let first = pull(&s, "pay", None).await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(modes(&first), (Some("mock"), Some("0.5")));
    let held = first.etag();
    let cached = pull(&s, "pay", Some(&held)).await;
    assert_eq!((cached.status, cached.etag()), (304, held.clone()));
    assert_eq!(modes(&cached), (Some("mock"), Some("0.5")));

    // `spec.mode` is set to `discovery`: still a 304 with the same ETag, and no factor.
    workload(&s, &s.ns, "pay-workload", false, 50);
    let switched = pull(&s, "pay", Some(&held)).await;
    assert_eq!((switched.status, switched.etag()), (304, held.clone()));
    assert_eq!(modes(&switched), (Some("discovery"), None));
    let full = pull(&s, "pay", None).await;
    assert_eq!(
        (full.status, full.etag(), full.text()),
        (200, held.clone(), first.text())
    );
    assert_eq!(modes(&full), (Some("discovery"), None));

    // A second `MimicWorkload` in `mock` turns it back to mock (factor 1: `1`).
    workload(&s, &s.ns, "second", true, 100);
    let again = pull(&s, "pay", Some(&held)).await;
    assert_eq!(
        (again.status, modes(&again)),
        (304, (Some("mock"), Some("1")))
    );

    // A pod of namespace `b` gets the mode of its own deployment, never that of `a`.
    let b = unique("elsewhere");
    workload(&s, &b, "pay-workload", false, 100);
    let tok = token(&b, "pay", "t");
    let other = request(&s.router, Method::GET, Some(&tok), "pay", "?format=1", None).await;
    assert_eq!(other.status, 200);
    assert_eq!(modes(&other), (Some("discovery"), None));
}

#[tokio::test]
async fn mck_08_refusals_carry_no_mode_header() {
    let s = setup(Options::default());
    workload(&s, &s.ns, "pay-workload", true, 100);
    let tok = token(&s.ns, "pay", "t");
    let steps = [
        (None, "?format=1", 401),
        (Some("other-pod"), "?format=1", 403),
        (Some(tok.as_str()), "?format=3", 400),
    ];
    for (credential, query, status) in steps {
        let answer = request(&s.router, Method::GET, credential, "pay", query, None).await;
        assert_eq!(answer.status, status);
        assert_eq!(modes(&answer), (None, None), "{status}");
    }
    let answer = request(
        &s.router,
        Method::POST,
        Some(&tok),
        "pay",
        "?format=1",
        None,
    )
    .await;
    assert_eq!((answer.status, modes(&answer)), (405, (None, None)));
}

#[tokio::test]
async fn mck_08_429_503_and_507_carry_no_mode_header() {
    let limited = setup(Options {
        rate: 1,
        ..Options::default()
    });
    workload(&limited, &limited.ns, "pay-workload", true, 100);
    let _ = pull(&limited, "pay", None).await;
    let _ = pull(&limited, "pay", None).await;
    let answer = pull(&limited, "pay", None).await;
    assert_eq!((answer.status, modes(&answer)), (429, (None, None)));

    let down = setup(Options {
        valkey: "redis://127.0.0.1:1".to_string(),
        ..Options::default()
    });
    workload(&down, &down.ns, "pay-workload", true, 100);
    let answer = pull(&down, "pay", None).await;
    assert_eq!((answer.status, modes(&answer)), (503, (None, None)));

    let large = setup(Options {
        max_egress: 64 << 10,
        ..Options::default()
    });
    workload(&large, &large.ns, "pay-workload", true, 100);
    push_report(&large, "pay", SESSION, 1, &super::egress::fat_report("k")).await;
    let answer = pull(&large, "pay", None).await;
    assert_eq!((answer.status, modes(&answer)), (507, (None, None)));
}

#[tokio::test]
async fn mck_08_each_pull_writes_pulls_with_the_expiry_of_pods_fields() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    workload(&s, &s.ns, "pay-workload", true, 100);
    let first = pull(&s, "pay", None).await;
    let before = pulls_when(&s, "pay", |p| p.contains_key(&pod_of("pay"))).await;
    let (mode, ms) = before[&pod_of("pay")].split_once(' ').unwrap();
    assert_eq!(mode, "mock");
    let ms: u64 = ms.parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(now.saturating_sub(ms) < 10_000, "{ms} vs {now}");
    let key = keys(&s.ns, "pay").unwrap().pulls;
    let mut c = conn().await;
    let field_ttl: Vec<i64> = redis::cmd("HPTTL")
        .arg(&key)
        .arg("FIELDS")
        .arg(1)
        .arg(pod_of("pay"))
        .query_async(&mut c)
        .await
        .unwrap();
    // `MIMIC_STORE_TTL` is 30 days in the test setup: the field lasts one day, at most.
    assert!((1..=86_400_000).contains(&field_ttl[0]), "{field_ttl:?}");
    let key_ttl: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(
        key_ttl > 86_400_000,
        "the key lives as long as the store TTL: {key_ttl}"
    );

    // A 304 writes it too, with the new mode.
    workload(&s, &s.ns, "pay-workload", false, 100);
    let cached = pull(&s, "pay", Some(&first.etag())).await;
    assert_eq!(cached.status, 304);
    let after = pulls_when(&s, "pay", |p| {
        p.get(&pod_of("pay"))
            .is_some_and(|v| v.starts_with("discovery "))
    })
    .await;
    assert!(after[&pod_of("pay")].starts_with("discovery "), "{after:?}");
    // Other deployments' keys are untouched.
    assert!(pulls(&s, "web").await.is_empty());
}

#[tokio::test]
async fn mck_08_a_failing_pulls_write_is_counted_and_the_pull_still_answered() {
    let s = setup(Options::default());
    workload(&s, &s.ns, "pay-workload", true, 50);
    // A string where the hash belongs: `HSET` is refused.
    let key = keys(&s.ns, "pay").unwrap().pulls;
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("x")
        .query_async(&mut conn().await)
        .await
        .unwrap();
    let answer = pull(&s, "pay", None).await;
    assert_eq!(answer.status, 200);
    assert_eq!(modes(&answer), (Some("mock"), Some("0.5")));
    // The write runs after the answer: wait for its task.
    wait_for(&s, "mimic_store_writes_total{result=\"error\"} 1").await;
}

async fn wait_for(s: &Setup, series: &str) {
    for _ in 0..200 {
        if s.metrics.encode().contains(series) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{series} never appeared:\n{}", s.metrics.encode());
}

#[tokio::test]
async fn mck_08_a_stalled_pulls_write_never_delays_the_answer_and_over_16_are_dropped() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    workload(&s, &s.ns, "pay-workload", true, 100);
    s.api.store.stall_pulls.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    for _ in 0..40 {
        let answer = pull(&s, "pay", None).await;
        assert_eq!(
            (answer.status, answer.header("mimic-mode")),
            (200, Some("mock"))
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    wait_for(&s, "mimic_store_writes_total{result=\"dropped\"} 24").await;
    // Released: the 16 in flight are written.
    s.api.store.stall_pulls.store(false, Ordering::SeqCst);
    for _ in 0..200 {
        if pulls(&s, "pay").await.contains_key(&pod_of("pay")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the writes in flight never finished");
}
