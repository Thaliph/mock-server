//! F9 step 4: builds, the single flight, the egress cache and the budgets of the egress route
//! (spec §7.7): PUL-09. PUL-12 (memory) is in `memory`.

use std::sync::atomic::Ordering::SeqCst;

use super::egress::{fat_report, hdel_meta, pull, push_report, request, set_outbound};
use super::v2::{AUTHORITY, outbound_report};
use super::*;

const OK: &str = "mimic_sidecar_api_egress_builds_total{result=\"ok\"}";
const CACHE_BYTES: &str = "mimic_sidecar_api_egress_cache_bytes";

fn count(s: &Setup, selector: &str) -> u64 {
    super::series(&s.metrics, selector).unwrap_or(0.0) as u64
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timeout waiting for {what}");
}

/// A pull whose answer is not read yet: the body stays unsent.
async fn raw_pull(s: &Setup, deployment: &str) -> Response {
    let mut request = Request::builder()
        .uri(format!("/v1/workloads/{deployment}/egress?format=1"))
        .header(
            "authorization",
            format!("Bearer {}", token(&s.ns, deployment, "t")),
        )
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(Peer(PEER.parse().unwrap()));
    s.router.clone().oneshot(request).await.unwrap()
}

/// The size of the answer for `outbound_report(&[AUTHORITY])`.
async fn projection_len() -> u64 {
    let probe = setup(Options::default());
    push_report(&probe, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    pull(&probe, "pay", None).await.bytes.len() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pul_09_32_concurrent_pulls_at_a_new_digest_make_one_build() {
    let s = Arc::new(setup(Options::default()));
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let before = count(&s, OK);
    let pulls: Vec<_> = (0..32)
        .map(|_| {
            let s = s.clone();
            tokio::spawn(async move { pull(&s, "pay", None).await })
        })
        .collect();
    let mut answers = Vec::new();
    for pull in pulls {
        answers.push(pull.await.unwrap());
    }
    assert!(answers.iter().all(|a| a.status == 200));
    assert!(answers.iter().all(|a| a.bytes == answers[0].bytes));
    assert!(answers.iter().all(|a| a.etag() == answers[0].etag()));
    assert_eq!(count(&s, OK), before + 1);
}

#[tokio::test]
async fn pul_09_the_cache_holds_the_budget_and_drops_the_least_recently_served() {
    let len = projection_len().await;
    let s = setup(Options {
        egress_budget: 2 * len,
        ..Options::default()
    });
    for deployment in ["pay", "web", "api"] {
        push_report(&s, deployment, SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    }
    for deployment in ["pay", "web"] {
        assert_eq!(pull(&s, deployment, None).await.status, 200);
    }
    assert_eq!(count(&s, CACHE_BYTES), 2 * len);
    assert_eq!(pull(&s, "api", None).await.status, 200);
    assert_eq!(count(&s, OK), 3);
    assert_eq!(count(&s, CACHE_BYTES), 2 * len, "within the budget");
    // `web` was served after `pay`: `api` took the place of `pay`.
    assert_eq!(pull(&s, "web", None).await.status, 200);
    assert_eq!(count(&s, OK), 3, "web is still cached");
    assert_eq!(pull(&s, "pay", None).await.status, 200);
    assert_eq!(count(&s, OK), 4, "pay is built again");
    assert_eq!(count(&s, CACHE_BYTES), 2 * len);
}

#[tokio::test]
async fn pul_09_a_projection_that_cannot_fit_while_others_are_sent_gets_a_503() {
    let len = projection_len().await;
    let s = setup(Options {
        egress_budget: len,
        ..Options::default()
    });
    for deployment in ["pay", "web"] {
        push_report(&s, deployment, SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    }
    let sending = raw_pull(&s, "pay").await;
    assert_eq!(sending.status(), 200);
    let refused = pull(&s, "web", None).await;
    assert_eq!(refused.status, 503);
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(s.api.budget.used(), 0, "the build share is released");
    // The answer is sent: its share goes back to the cache, which makes room.
    drop(sending);
    assert_eq!(pull(&s, "web", None).await.status, 200);
    assert_eq!(count(&s, CACHE_BYTES), len);
}

#[tokio::test]
async fn pul_09_a_build_needs_the_body_budget_and_the_cache_and_304_do_not() {
    let s = setup(Options::default());
    for deployment in ["pay", "web"] {
        push_report(&s, deployment, SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    }
    let warm = pull(&s, "pay", None).await;
    assert_eq!(warm.status, 200);
    let whole = s.api.budget.reserve(s.api.budget.capacity()).unwrap();
    let cached = pull(&s, "pay", None).await;
    assert_eq!((cached.status, &cached.bytes), (200, &warm.bytes));
    assert_eq!(pull(&s, "pay", Some(&warm.etag())).await.status, 304);
    let spent = pull(&s, "web", None).await;
    assert_eq!(spent.status, 503);
    assert_eq!(spent.header("retry-after"), Some("1"));
    // A stale digest (an F8 write) takes the same share, so it is refused too.
    hdel_meta(&s, "pay", "outbound_digest").await;
    assert_eq!(pull(&s, "pay", None).await.status, 503);
    drop(whole);
    assert_eq!(pull(&s, "pay", None).await.status, 200);
    assert_eq!(pull(&s, "web", None).await.status, 200);
    assert_eq!(s.api.budget.used(), 0);
}

#[tokio::test]
async fn pul_09_the_share_is_released_after_a_507_and_after_a_500() {
    let s = setup(Options {
        max_egress: 64 << 10,
        ..Options::default()
    });
    push_report(&s, "web", SESSION, 1, &fat_report("x")).await;
    assert_eq!(pull(&s, "web", None).await.status, 507);
    assert_eq!(s.api.budget.used(), 0);
    assert_eq!(count(&s, CACHE_BYTES), 0);
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    set_outbound(&s, "pay", "{\"version\":2,\"bogus\":1}").await;
    hdel_meta(&s, "pay", "outbound_digest").await;
    assert_eq!(pull(&s, "pay", None).await.status, 500);
    assert_eq!(s.api.budget.used(), 0);
    assert_eq!(count(&s, CACHE_BYTES), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pul_09_a_client_that_disconnects_keeps_the_share_until_the_build_ends() {
    let s = Arc::new(setup(Options::default()));
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    s.api.egress.pause.store(true, SeqCst);
    let client = {
        let s = s.clone();
        tokio::spawn(async move { pull(&s, "pay", None).await })
    };
    until("the build to run", || s.api.egress.running() == 1).await;
    assert!(s.api.budget.used() > 0);
    client.abort();
    let _ = client.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        s.api.budget.used() > 0,
        "the share was released while the build runs"
    );
    s.api.egress.pause.store(false, SeqCst);
    until("the build to end", || s.api.budget.used() == 0).await;
    // The build was not lost: the next pull is answered from the cache.
    assert_eq!(pull(&s, "pay", None).await.status, 200);
    assert_eq!(count(&s, OK), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pul_09_at_most_two_builds_run_at_once() {
    let s = Arc::new(setup(Options::default()));
    let names = ["d1", "d2", "d3", "d4", "d5"];
    for deployment in names {
        push_report(&s, deployment, SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    }
    s.api.egress.pause.store(true, SeqCst);
    let pulls: Vec<_> = names
        .into_iter()
        .map(|deployment| {
            let s = s.clone();
            tokio::spawn(async move { pull(&s, deployment, None).await })
        })
        .collect();
    until("two builds to run", || s.api.egress.running() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.api.egress.running(), 2);
    s.api.egress.pause.store(false, SeqCst);
    for pull in pulls {
        assert_eq!(pull.await.unwrap().status, 200);
    }
    assert_eq!(s.api.egress.peak(), 2);
}

#[tokio::test]
async fn pul_09_two_namespaces_never_get_each_others_projection() {
    let (a, b) = (setup(Options::default()), setup(Options::default()));
    push_report(&a, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let other = outbound_report(&["other.example.com:443"]);
    push_report(&b, "pay", SESSION, 1, &other).await;
    // Both namespaces are pulled through the replica of `a`: one cache.
    let tokens = [token(&a.ns, "pay", "t"), token(&b.ns, "pay", "t")];
    let mut seen = Vec::new();
    for _ in 0..2 {
        for tok in &tokens {
            let answer = request(&a.router, Method::GET, Some(tok), "pay", "?format=1", None).await;
            assert_eq!(answer.status, 200);
            seen.push(answer.text());
        }
    }
    assert!(seen[0].contains(AUTHORITY) && !seen[0].contains("other.example.com"));
    assert!(seen[1].contains("other.example.com") && !seen[1].contains(AUTHORITY));
    assert_eq!((&seen[0], &seen[1]), (&seen[2], &seen[3]));
}

#[tokio::test]
async fn pul_09_every_if_none_match_value_and_header_is_read() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let etag = pull(&s, "pay", None).await.etag();
    let tok = token(&s.ns, "pay", "t");
    for values in [
        vec![format!("\"1-nope\", {etag}")],
        vec!["\"1-nope\"".to_string(), etag.clone()],
        vec![etag.clone(), "\"1-nope\"".to_string()],
    ] {
        let mut builder = Request::builder()
            .uri("/v1/workloads/pay/egress?format=1")
            .header("authorization", format!("Bearer {tok}"));
        for value in &values {
            builder = builder.header("if-none-match", value);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        request.extensions_mut().insert(Peer(PEER.parse().unwrap()));
        let response = s.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 304, "{values:?}");
    }
}
