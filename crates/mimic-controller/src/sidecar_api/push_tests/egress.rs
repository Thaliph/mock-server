//! F9 step 3: the egress route of the sidecar API (spec §7.7): PUL-06 (route half), PUL-07,
//! PUL-08 and PUL-10. PUL-11 (configuration) is tested in `push_config`.

use axum::http::HeaderMap;
use mimic_core::discovery::egress::{EgressSet, etag};
use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;
use sha2::{Digest, Sha256};

use super::v2::{AUTHORITY, outbound_report};
use super::*;

pub(super) const EGRESS: &str = "/v1/workloads/{deployment}/egress";
const EMPTY: &str = r#"{"authorities":{},"format":1}"#;
/// Never in an answer, a log line or a metric.
const CANARY: &str = "CANARYSECRET-91c4e7";

pub(super) struct Pulled {
    pub(super) status: u16,
    pub(super) headers: HeaderMap,
    pub(super) bytes: Vec<u8>,
}

impl Pulled {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub(super) fn etag(&self) -> String {
        self.header("etag").expect("an ETag").to_string()
    }

    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// One request to the egress route.
pub(super) async fn request(
    router: &Router,
    method: Method,
    token: Option<&str>,
    deployment: &str,
    query: &str,
    none_match: Option<&str>,
) -> Pulled {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/v1/workloads/{deployment}/egress{query}"));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(value) = none_match {
        request = request.header("if-none-match", value);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request.extensions_mut().insert(Peer(PEER.parse().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Pulled {
        status,
        headers,
        bytes: bytes.to_vec(),
    }
}

/// A pull by the pod of `deployment` in the namespace of `s`, `format=1`.
pub(super) async fn pull(s: &Setup, deployment: &str, none_match: Option<&str>) -> Pulled {
    let tok = token(&s.ns, deployment, "t");
    request(
        &s.router,
        Method::GET,
        Some(&tok),
        deployment,
        "?format=1",
        none_match,
    )
    .await
}

pub(super) async fn stored_outbound(s: &Setup, deployment: &str) -> Option<String> {
    let key = keys(&s.ns, deployment).unwrap().outbound;
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut conn().await)
        .await
        .unwrap()
}

pub(super) async fn set_outbound(s: &Setup, deployment: &str, bytes: &str) {
    let key = keys(&s.ns, deployment).unwrap().outbound;
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(bytes)
        .query_async(&mut conn().await)
        .await
        .unwrap();
}

pub(super) async fn hset_meta(s: &Setup, deployment: &str, field: &str, value: &str) {
    let key = keys(&s.ns, deployment).unwrap().meta;
    let _: i64 = redis::cmd("HSET")
        .arg(key)
        .arg(field)
        .arg(value)
        .query_async(&mut conn().await)
        .await
        .unwrap();
}

pub(super) async fn hdel_meta(s: &Setup, deployment: &str, field: &str) {
    let key = keys(&s.ns, deployment).unwrap().meta;
    let _: i64 = redis::cmd("HDEL")
        .arg(key)
        .arg(field)
        .query_async(&mut conn().await)
        .await
        .unwrap();
}

fn hex_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the route must answer for stored `:outbound` bytes.
fn projection_of(outbound: &str) -> String {
    let report = Report::parse_checked(outbound, usize::MAX, usize::MAX).expect("a stored part");
    EgressSet::project_as(&report, 1).to_json().unwrap()
}

pub(super) async fn push_report(
    s: &Setup,
    deployment: &str,
    session: &str,
    sequence: u64,
    report: &Report,
) {
    let answer = push(s, deployment, payload(session, sequence, report)).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// An outbound report whose projection is large: 64 authorities, two statuses each, with a
/// sample of about 1 KiB per status. Records `secret` as a request header and body field.
pub(super) fn fat_report(secret: &str) -> Report {
    let mut report = Report::new(Some(pod_of("pay")));
    let filler = "x".repeat(1000);
    let body = format!(r#"{{"note":"{filler}","password":"{secret}"}}"#);
    let auth = format!("Bearer {secret}");
    let req_headers = [
        ("authorization", auth.as_str()),
        ("content-type", "application/json"),
    ];
    let resp_headers = [("content-type", "application/json")];
    for n in 0..64 {
        for status in [200, 404] {
            report.record_at(
                ScopeKey::Authority(format!("a{n}.example.com:443")),
                &Via {
                    tls: true,
                    sni: None,
                    dst: None,
                },
                &LIMITS,
                &Redactor::default(),
                &Exchange {
                    direction: Direction::Outbound,
                    port: 443,
                    method: "GET",
                    path: "/v1/x",
                    status,
                    head_ms: 3,
                    host: None,
                    grpc: false,
                    observation: Observation {
                        query: "",
                        req_headers: &req_headers,
                        req_body: Some(body.as_bytes()),
                        resp_headers: &resp_headers,
                        resp_body: Some(body.as_bytes()),
                        resp_trailers: &[],
                        resp_marks: &[],
                    },
                },
            );
        }
    }
    report
}

#[tokio::test]
async fn pul_06_after_an_f8_style_write_the_route_answers_the_stored_bytes_with_their_etag() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let first = pull(&s, "pay", None).await;
    assert_eq!(first.status, 200, "{}", first.text());

    // Another deployment's outbound bytes stand in for what an F8 controller would write.
    push_report(
        &s,
        "web",
        SESSION,
        1,
        &outbound_report(&["other.example.com:443"]),
    )
    .await;
    let foreign = stored_outbound(&s, "web").await.unwrap();
    set_outbound(&s, "pay", &foreign).await;
    let expected = (
        projection_of(&foreign),
        etag(1, &hex_of(foreign.as_bytes())),
    );

    // The digest was removed.
    hdel_meta(&s, "pay", "outbound_digest").await;
    let removed = pull(&s, "pay", Some(&first.etag())).await;
    assert_eq!(removed.status, 200);
    assert_eq!((removed.text(), removed.etag()), expected);
    // The stale digest gets no 304, not even for the ETag of the bytes themselves.
    let again = pull(&s, "pay", Some(&expected.1)).await;
    assert_eq!(again.status, 200, "a stale digest never gives a 304");

    // A digest of an older version, with the hash of the earlier bytes.
    let version: u64 = {
        let meta = stored_meta(&s, "pay").await;
        meta["version"].parse().unwrap()
    };
    let old = format!("{}:{}", version - 1, "0".repeat(64));
    hset_meta(&s, "pay", "outbound_digest", &old).await;
    let older = pull(&s, "pay", Some(&first.etag())).await;
    assert_eq!(older.status, 200);
    assert_eq!((older.text(), older.etag()), expected);
}

#[tokio::test]
async fn pul_07_the_route_serves_the_projection_and_a_304_without_reading_outbound() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let stored = stored_outbound(&s, "pay").await.unwrap();

    let first = pull(&s, "pay", None).await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(first.header("content-type"), Some("application/json"));
    assert_eq!(first.header("cache-control"), Some("no-store"));
    assert_eq!(first.text(), projection_of(&stored));
    assert_eq!(first.etag(), etag(1, &hex_of(stored.as_bytes())));

    // `:outbound` is unreadable now: only an answer that never reads it can be a 304.
    set_outbound(&s, "pay", "not a report").await;
    let listed = format!("\"1-other\", {}", first.etag());
    for none_match in [first.etag(), listed] {
        let cached = pull(&s, "pay", Some(&none_match)).await;
        assert_eq!(cached.status, 304, "{none_match}");
        assert_eq!(cached.etag(), first.etag());
        assert!(cached.bytes.is_empty());
    }
    // Weak validators and `*` do not match: a 200, from the cache (step 4), which holds the
    // projection of this digest and never reads the damaged bytes.
    for none_match in [format!("W/{}", first.etag()), "*".to_string()] {
        let answer = pull(&s, "pay", Some(&none_match)).await;
        assert_eq!((answer.status, answer.text()), (200, first.text()));
    }
    set_outbound(&s, "pay", &stored).await;

    // A version 2 push that changes the outbound part: a new ETag.
    let next = outbound_report(&["b.example.com:443"]);
    push_report(&s, "pay", SESSION, 2, &next).await;
    let changed = pull(&s, "pay", Some(&first.etag())).await;
    assert_eq!(changed.status, 200);
    assert_ne!(changed.etag(), first.etag());
    let stored = stored_outbound(&s, "pay").await.unwrap();
    assert_eq!(changed.text(), projection_of(&stored));
    // A version 1 push leaves `:outbound` alone: still a 304.
    push_report(&s, "pay", SESSION_2, 1, &report_of(&["/b"])).await;
    let kept = pull(&s, "pay", Some(&changed.etag())).await;
    assert_eq!((kept.status, kept.etag()), (304, changed.etag()));
}

#[tokio::test]
async fn pul_07_a_deployment_without_outbound_data_gets_the_empty_set() {
    let s = setup(Options::default());
    let ghost = pull(&s, "ghost", None).await;
    assert_eq!((ghost.status, ghost.text()), (200, EMPTY.to_string()));
    assert!(ghost.header("etag").is_some());
    push_report(&s, "web", SESSION, 1, &report_of(&["/a"])).await;
    let inbound = pull(&s, "web", None).await;
    assert_eq!((inbound.status, inbound.text()), (200, EMPTY.to_string()));
    assert_eq!(inbound.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn pul_07_two_replicas_answer_the_same_etag_and_bytes() {
    let first = setup(Options::default());
    let second = setup(Options::default());
    push_report(&first, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    // The second replica serves the namespace of the first (the token decides it).
    let tok = token(&first.ns, "pay", "t");
    let a = pull(&first, "pay", None).await;
    let b = request(
        &second.router,
        Method::GET,
        Some(&tok),
        "pay",
        "?format=1",
        None,
    )
    .await;
    assert_eq!(b.status, 200, "{}", b.text());
    assert_eq!((a.etag(), a.bytes), (b.etag(), b.bytes));
}

#[tokio::test]
async fn pul_08_refusals_by_identity_and_request() {
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    let get = |token: Option<&'static str>, deployment: &'static str, query: &'static str| {
        let router = s.router.clone();
        async move { request(&router, Method::GET, token, deployment, query, None).await }
    };
    // Authentication.
    assert_eq!(get(None, "pay", "?format=1").await.status, 401);
    assert_eq!(get(Some("bad"), "pay", "?format=1").await.status, 401);
    assert_eq!(
        get(Some("wrong-audience"), "pay", "?format=1").await.status,
        401
    );
    // Authorisation: another deployment's pod, no bound pod.
    assert_eq!(get(Some("other-pod"), "pay", "?format=1").await.status, 403);
    assert_eq!(get(Some("no-claim"), "pay", "?format=1").await.status, 403);
    // The request: `format`, the name.
    let tok = token(&s.ns, "pay", "t");
    for query in ["", "?format=3", "?format=abc", "?other=1", "?format="] {
        let answer = request(&s.router, Method::GET, Some(&tok), "pay", query, None).await;
        assert_eq!(answer.status, 400, "{query}");
    }
    let bad = token(&s.ns, "Pay_X", "t");
    let answer = request(
        &s.router,
        Method::GET,
        Some(&bad),
        "Pay_X",
        "?format=1",
        None,
    )
    .await;
    assert_eq!(answer.status, 400);
    // Other methods.
    for method in [Method::POST, Method::PUT] {
        let answer = request(&s.router, method, Some(&tok), "pay", "?format=1", None).await;
        assert_eq!(answer.status, 405);
        assert_eq!(answer.header("allow"), Some("GET"));
    }
    // A pod of namespace `b` naming a deployment that has data in namespace `a` only.
    let other = token(&unique("elsewhere"), "pay", "t");
    let answer = request(
        &s.router,
        Method::GET,
        Some(&other),
        "pay",
        "?format=1",
        None,
    )
    .await;
    assert_eq!((answer.status, answer.text()), (200, EMPTY.to_string()));
}

#[tokio::test]
async fn pul_08_the_rate_limit_comes_before_the_token_review() {
    // 1 request per second: a burst of 2.
    let s = setup(Options {
        rate: 1,
        ..Options::default()
    });
    assert_eq!(pull(&s, "pay", None).await.status, 200);
    assert_eq!(pull(&s, "pay", None).await.status, 200);
    let reviews = s.reviews.lock().unwrap().len();
    let tok = "never-reviewed";
    let answer = request(&s.router, Method::GET, Some(tok), "pay", "?format=1", None).await;
    assert_eq!(answer.status, 429);
    assert_eq!(answer.header("retry-after"), Some("1"));
    assert_eq!(s.reviews.lock().unwrap().len(), reviews, "no TokenReview");
}

#[tokio::test]
async fn pul_08_valkey_stopped_is_a_503_with_retry_after() {
    let s = setup(Options {
        valkey: "redis://127.0.0.1:1".to_string(),
        ..Options::default()
    });
    let answer = pull(&s, "pay", None).await;
    assert_eq!(answer.status, 503);
    assert_eq!(answer.header("retry-after"), Some("1"));
    assert!(answer.bytes.is_empty());
}

#[tokio::test]
async fn pul_08_a_corrupted_outbound_is_a_500_that_is_logged_and_never_overwritten() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options::default());
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    set_outbound(&s, "pay", "{\"version\":2,\"bogus\":1}").await;
    hdel_meta(&s, "pay", "outbound_digest").await;
    let answer = pull(&s, "pay", None).await;
    assert_eq!(answer.status, 500);
    assert_eq!(
        stored_outbound(&s, "pay").await.as_deref(),
        Some("{\"version\":2,\"bogus\":1}")
    );
    let errors: Vec<Value> = super::lines(&capture)
        .into_iter()
        .filter(|l| l["level"] == "ERROR")
        .collect();
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0]["message"], "stored report unreadable");
    assert_eq!(errors[0]["key"], "outbound");
    assert_eq!(errors[0]["deployment"], "pay");
}

#[tokio::test]
async fn pul_08_a_projection_over_the_limit_is_a_507_warned_once_per_deployment() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options {
        max_egress: 64 << 10,
        ..Options::default()
    });
    push_report(&s, "pay", SESSION, 1, &fat_report(CANARY)).await;
    let stored = stored_outbound(&s, "pay").await.unwrap();
    assert!(
        projection_of(&stored).len() > 64 << 10,
        "the premise of the test"
    );
    for _ in 0..2 {
        let answer = pull(&s, "pay", None).await;
        assert_eq!(answer.status, 507);
        assert!(answer.header("etag").is_none());
    }
    let warns: Vec<Value> = super::lines(&capture)
        .into_iter()
        .filter(|l| l["message"] == "egress limit reached")
        .collect();
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(warns[0]["level"], "WARN");
    assert_eq!(warns[0]["reason"], "projection");
    assert_eq!(warns[0]["deployment"], "pay");
    assert_eq!(warns[0]["namespace"], s.ns.as_str());
    let result = format!(
        "mimic_sidecar_api_egress_builds_total{{result=\"too_large\"}} {}",
        2
    );
    assert!(
        s.metrics.encode().contains(&result),
        "{}",
        s.metrics.encode()
    );
}

#[tokio::test]
async fn pul_08_a_stored_outbound_that_could_never_be_built_is_a_507_without_a_read() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    // The push goes through a replica with room; the pulls through one whose whole budget is
    // below STRLEN plus the 64 KiB of the projection.
    let s = setup(Options::default());
    let small = setup(Options {
        max_egress: 64 << 10,
        budget: 256 << 10,
        ..Options::default()
    });
    push_report(&s, "pay", SESSION, 1, &fat_report(CANARY)).await;
    let stored = stored_outbound(&s, "pay").await.unwrap();
    assert!(stored.len() > 192 << 10, "the premise of the test");
    let tok = token(&s.ns, "pay", "t");
    for _ in 0..2 {
        let answer = request(
            &small.router,
            Method::GET,
            Some(&tok),
            "pay",
            "?format=1",
            None,
        )
        .await;
        assert_eq!(answer.status, 507);
        assert!(answer.header("retry-after").is_none());
    }
    let warns: Vec<Value> = super::lines(&capture)
        .into_iter()
        .filter(|l| l["message"] == "egress limit reached")
        .collect();
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert_eq!(warns[0]["reason"], "stored");
    assert!(
        small
            .metrics
            .encode()
            .contains("mimic_sidecar_api_egress_builds_total{result=\"too_large\"} 2")
    );
}

#[tokio::test]
async fn pul_10_one_line_per_pull_and_the_series_follow_without_a_secret() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(Options {
        max_egress: 64 << 10,
        ..Options::default()
    });
    push_report(&s, "pay", SESSION, 1, &outbound_report(&[AUTHORITY])).await;
    push_report(&s, "web", SESSION, 1, &fat_report(CANARY)).await;
    let tok = token(&s.ns, "pay", "t");
    let first = pull(&s, "pay", None).await;
    let cached = pull(&s, "pay", Some(&first.etag())).await;
    assert_eq!(cached.status, 304);
    let steps = [
        (None, "pay", "?format=1", 401),
        (Some("other-pod"), "pay", "?format=1", 403),
        (Some(tok.as_str()), "pay", "?format=3", 400),
    ];
    for (credential, deployment, query, status) in steps {
        let answer = request(&s.router, Method::GET, credential, deployment, query, None).await;
        assert_eq!(answer.status, status);
    }
    assert_eq!(pull(&s, "web", None).await.status, 507);

    let all = super::lines(&capture);
    let api: Vec<&Value> = all
        .iter()
        .filter(|l| l["target"] == "sidecar_api" && l["endpoint"] == EGRESS)
        .collect();
    let results: Vec<&str> = api.iter().map(|l| l["result"].as_str().unwrap()).collect();
    assert_eq!(
        results,
        [
            "ok",
            "not_modified",
            "unauthenticated",
            "forbidden",
            "invalid",
            "egress_limit"
        ],
        "{api:#?}"
    );
    assert_eq!(api[0]["bytes"], first.bytes.len());
    assert_eq!(api[1]["bytes"], 0);
    for line in [api[0], api[1], api[5]] {
        assert_eq!(line["namespace"], s.ns.as_str());
        assert_eq!(
            line["pod"],
            pod_of(if line == api[5] { "web" } else { "pay" })
        );
    }
    assert_eq!(api[0]["deployment"], "pay");
    assert_eq!(api[4]["reason"], "format");

    let metrics = s.metrics.encode();
    for (result, count) in [
        ("ok", 1),
        ("not_modified", 1),
        ("unauthenticated", 1),
        ("forbidden", 1),
        ("invalid", 1),
        ("egress_limit", 1),
    ] {
        let selector = format!(
            "mimic_sidecar_api_requests_total{{endpoint=\"{EGRESS}\",result=\"{result}\"}}"
        );
        assert_eq!(super::series(&s.metrics, &selector), Some(count.into()));
    }

    // No secret, no hash of one, no token, no template or authority in any series.
    let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let hash = xxh3_64(CANARY.as_bytes());
    let needles = [
        CANARY.to_string(),
        hash.to_string(),
        format!("{hash:x}"),
        tok.clone(),
    ];
    for needle in &needles {
        assert!(!raw.contains(needle.as_str()), "a secret in the logs");
        assert!(!metrics.contains(needle.as_str()), "a secret in /metrics");
        assert!(!first.text().contains(needle.as_str()), "a secret served");
    }
    for needle in [AUTHORITY, "/v1/x", "example.com"] {
        assert!(!metrics.contains(needle), "{needle} in /metrics");
    }
}
