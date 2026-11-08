//! F11 step 6: the load API (LDT-08) and the outputs of the controller's part of a run
//! (LDT-14). Needs the Valkey of `MIMIC_TEST_VALKEY_URL`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::http::{Method, Request};
use http_body_util::BodyExt;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use mimic_core::load::job::job_name;
use mimic_core::load::summary::{PodResults, Stats};
use mimic_runtime::metrics::Metrics;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::load::*;
use super::*;
use crate::store::load::{Admit, AdmitLimits, Ended, MAX_POST};
use crate::store::{Store, StoreConfig, StoreMetrics};
use crate::workloads::{MockSettings, WorkloadCache};

const PEER: &str = "10.1.1.1";
const CANARY: &str = "canary-load-5d71c0e9";
const TICKET: &str = "v1.eyJ0aWNrZXQiOjF9.c2lnbmF0dXJlLW9mLXRoZS10aWNrZXQ";
const NAME: &str = "load-1";
const DEPLOYMENT: &str = "pay";

fn unique(prefix: &str) -> String {
    let mut bytes = [0u8; 6];
    rand::fill(&mut bytes[..]);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

fn valkey_url() -> String {
    std::env::var("MIMIC_TEST_VALKEY_URL").unwrap_or_else(|_| {
        panic!("MIMIC_TEST_VALKEY_URL is not set: start a Valkey >= 9.0 and set it")
    })
}

/// Accepts the tokens `<namespace>|<pod>|<audience>|<tag>` that carry its own audience.
struct Fake {
    audience: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Reviewer for Fake {
    fn review<'a>(&'a self, token: &'a str) -> ReviewFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let parts: Vec<&str> = token.split('|').collect();
            if parts.len() != 4 || parts[2] != self.audience {
                return Ok(Review {
                    authenticated: false,
                    username: String::new(),
                    pod: None,
                });
            }
            Ok(Review {
                authenticated: true,
                username: format!("system:serviceaccount:{}:default", parts[0]),
                pod: Some(parts[1].to_string()),
            })
        })
    }
}

struct Setup {
    router: Router,
    api: Arc<SidecarApi>,
    metrics: Arc<Metrics>,
    ns: String,
    uid: String,
    job: String,
    runs: Arc<RunIndex>,
    slices: Arc<SliceCache>,
    workloads: Arc<WorkloadCache>,
    load_calls: Arc<AtomicUsize>,
    sidecar_calls: Arc<AtomicUsize>,
}

fn run_info(ns: &str, uid: &str) -> RunInfo {
    RunInfo {
        namespace: ns.to_string(),
        name: NAME.to_string(),
        uid: uid.to_string(),
        deployment: DEPLOYMENT.to_string(),
        concurrency: 300,
        rate: Some(1001),
        duration_ms: 60_000,
        ramp_up_ms: 5_000,
        timeout_ms: 10_000,
        start_timeout_ms: 300_000,
        pod_concurrency: 128,
        stop: false,
    }
}

async fn setup(rate: u32) -> Setup {
    let ns = unique("shop");
    let mut hex = [0u8; 4];
    rand::fill(&mut hex[..]);
    let uid = format!(
        "{}-6a3f-4c58-9e11-2f7d3a8b4c60",
        hex.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let job = job_name(&uid);
    let mut metrics = Metrics::new("controller", "test");
    let store = Store::new(
        redis::Client::open(valkey_url()).expect("a valid URL"),
        StoreConfig {
            ttl: Duration::from_secs(3600),
            limits: mimic_core::discovery::report::Limits {
                max_literals: 32,
                max_endpoints: 256,
                max_authorities: 64,
                sample_budget: 1 << 20,
            },
            max_deployments: 50,
        },
        StoreMetrics::register(metrics.registry_mut()),
    );
    let api_metrics = ApiMetrics::register(metrics.registry_mut());
    let workloads = Arc::new(WorkloadCache::synced_with(&[(&ns, "pay-w", DEPLOYMENT)]));
    workloads.set_mock(
        &ns,
        "pay-w",
        MockSettings {
            mock: true,
            ..MockSettings::default()
        },
    );
    let runs = Arc::new(RunIndex::default());
    runs.apply(run_info(&ns, &uid));
    let slices = Arc::new(SliceCache::default());
    let (load_calls, sidecar_calls) =
        (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let api = Arc::new(SidecarApi {
        seed: Arc::new(SeedSlot::new()),
        reviewer: Arc::new(Fake {
            audience: AUDIENCE,
            calls: sidecar_calls.clone(),
        }),
        metrics: api_metrics.clone(),
        limiter: ReviewLimiter::default(),
        rate: RateLimiter::new(rate),
        reviews: ReviewCache::default(),
        budget: BodyBudget::new(1 << 30),
        store,
        workloads: workloads.clone(),
        max_report: 1 << 20,
        max_push_endpoints: 256,
        max_egress: 4 << 20,
        egress: EgressCache::new(32 << 20, &api_metrics),
        egress_logged: EgressLogged::default(),
        load: Some(LoadApi {
            reviewer: Arc::new(Fake {
                audience: LOAD_AUDIENCE,
                calls: load_calls.clone(),
            }),
            reviews: ReviewCache::default(),
            runs: runs.clone(),
            slices: slices.clone(),
        }),
    });
    Setup {
        router: router(api.clone()),
        api,
        metrics: Arc::new(metrics),
        ns,
        uid,
        job,
        runs,
        slices,
        workloads,
        load_calls,
        sidecar_calls,
    }
}

impl Setup {
    /// Admits the run with a plan.
    async fn admit(&self, plan: &str) {
        self.api
            .store
            .load_admit(&Admit {
                namespace: &self.ns,
                deployment: DEPLOYMENT,
                uid: &self.uid,
                spec: r#"{"name":"load-1"}"#,
                plan,
                ticket: TICKET,
                now_ms: 1_000_000,
                deadline_ms: 2_000_000,
                reserve: 1 << 20,
                limits: AdmitLimits {
                    max_runs: 100_000,
                    max_namespace_runs: 100_000,
                    budget: 1 << 40,
                },
            })
            .await
            .expect("admitted");
    }

    fn pod(&self, index: u32) -> String {
        format!("{}-{index}-bcdfg", self.job)
    }

    /// A load token of pod `pod` in namespace `ns`.
    fn token(&self, ns: &str, pod: &str, audience: &str) -> String {
        format!("{ns}|{pod}|{audience}|{}", unique("t"))
    }

    fn load_token(&self, index: u32) -> String {
        self.token(&self.ns, &self.pod(index), LOAD_AUDIENCE)
    }

    fn url(&self, route: &str) -> String {
        format!("/v1/load/{NAME}/{}/{route}", self.uid)
    }

    fn slice(&self, pods: &[(&str, &str, bool)]) {
        let endpoints: Vec<Value> = pods
            .iter()
            .map(|(pod, ip, ready)| {
                json!({"addresses": [ip], "conditions": {"ready": ready},
                       "targetRef": {"kind": "Pod", "name": pod}})
            })
            .collect();
        let slice: EndpointSlice = serde_json::from_value(json!({
            "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
            "metadata": {"name": "pay-abc", "namespace": self.ns},
            "addressType": "IPv4", "endpoints": endpoints,
        }))
        .expect("a slice");
        self.slices.apply(slice);
    }
}

struct Answer {
    status: u16,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<(Vec<u8>, Option<usize>)>,
) -> Answer {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some((bytes, length)) => {
            if let Some(length) = length {
                request = request.header("content-length", length.to_string());
            }
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let mut request = request.body(body).unwrap();
    request.extensions_mut().insert(Peer(PEER.parse().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        bytes: bytes.to_vec(),
    }
}

async fn get(s: &Setup, route: &str, token: &str) -> Answer {
    call(&s.router, Method::GET, &s.url(route), Some(token), None).await
}

fn results(requests: u64) -> Value {
    let mut stats = Stats::default();
    for _ in 0..requests {
        stats.response(200, 1500);
    }
    json!(PodResults {
        endpoints: vec![stats]
    })
}

fn post_body(pod: &str, final_post: bool, requests: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "final": final_post, "pod": pod, "reason": null, "results": results(requests),
    }))
    .unwrap()
}

async fn post(s: &Setup, token: &str, body: Vec<u8>) -> Answer {
    let length = body.len();
    call(
        &s.router,
        Method::POST,
        &s.url("results"),
        Some(token),
        Some((body, Some(length))),
    )
    .await
}

const PLAN: &str = r#"{"credentials":[],"endpoints":[]}"#;

#[tokio::test]
async fn ldt_08_the_plan_comes_with_the_ticket_the_worker_and_the_pod_params() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    // 300 over 128 per pod: 3 pods of 100, rates 334, 334, 333.
    let answer = get(&s, "plan", &s.load_token(2)).await;
    assert_eq!(
        answer.status,
        200,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    let body = answer.json();
    assert_eq!(body["plan"], serde_json::from_str::<Value>(PLAN).unwrap());
    assert_eq!(body["ticket"], TICKET);
    assert_eq!(body["worker"], 2);
    assert_eq!(body["workers"], 3);
    assert_eq!(
        body["params"],
        json!({"concurrency": 100, "rate": 333, "durationMs": 60_000, "rampUpMs": 5_000,
               "timeoutMs": 10_000, "startTimeoutMs": 300_000})
    );
    let first = get(&s, "plan", &s.load_token(0)).await.json();
    assert_eq!(first["worker"], 0);
    assert_eq!(first["params"]["rate"], 334);

    // The closed model has no rate.
    let mut closed = run_info(&s.ns, &s.uid);
    closed.rate = None;
    s.runs.apply(closed);
    let body = get(&s, "plan", &s.load_token(1)).await.json();
    assert_eq!(body["params"]["rate"], Value::Null);
}

#[tokio::test]
async fn ldt_08_the_plan_stored_at_admission_is_served_after_the_report_changed() {
    let s = setup(10_000).await;
    let plan = r#"{"credentials":[],"endpoints":[]}"#;
    s.admit(plan).await;
    // The report of the deployment changes after the admission.
    let report = mimic_core::discovery::report::Report::new(Some("pay-7d9f8b6c4d-x2k9p".into()));
    s.api
        .store
        .push(
            &s.ns,
            DEPLOYMENT,
            crate::store::Push {
                pod: "pay-7d9f8b6c4d-x2k9p".into(),
                seq: None,
                json: report.to_json().unwrap(),
            },
        )
        .await
        .expect("pushed");
    let body = get(&s, "plan", &s.load_token(0)).await.json();
    assert_eq!(body["plan"], serde_json::from_str::<Value>(plan).unwrap());
}

#[tokio::test]
async fn ldt_08_audiences_are_not_interchangeable() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    // A sidecar token (audience `mimic.platform.io`) is 401 on every load route.
    let sidecar = s.token(&s.ns, &s.pod(0), AUDIENCE);
    for route in ["plan", "targets"] {
        assert_eq!(get(&s, route, &sidecar).await.status, 401, "{route}");
    }
    assert_eq!(
        post(&s, &sidecar, post_body(&s.pod(0), false, 1))
            .await
            .status,
        401
    );
    // A load token is 401 on the report and the egress routes, which the sidecars use.
    let load = s.load_token(0);
    let report = call(
        &s.router,
        Method::POST,
        "/v1/workloads/pay/report",
        Some(&load),
        Some((b"{}".to_vec(), Some(2))),
    )
    .await;
    assert_eq!(report.status, 401);
    let egress = call(
        &s.router,
        Method::GET,
        "/v1/workloads/pay/egress",
        Some(&load),
        None,
    )
    .await;
    assert_eq!(egress.status, 401);
    // And a load token sitting in the cache of one does not open the other.
    assert_eq!(get(&s, "plan", &load).await.status, 200);
    assert_eq!(report_status(&s, &load).await, 401);
    // No token at all.
    let anonymous = call(&s.router, Method::GET, &s.url("plan"), None, None).await;
    assert_eq!(anonymous.status, 401);
}

async fn report_status(s: &Setup, token: &str) -> u16 {
    call(
        &s.router,
        Method::POST,
        "/v1/workloads/pay/report",
        Some(token),
        Some((b"{}".to_vec(), Some(2))),
    )
    .await
    .status
}

#[tokio::test]
async fn ldt_08_pod_namespace_and_run_are_checked() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    // A pod of another Job, one beyond the Job's pods, one that is no generated name.
    let other_job = format!(
        "{}-0-bcdfg",
        job_name("ffffffff-6a3f-4c58-9e11-2f7d3a8b4c60")
    );
    for pod in [
        other_job.as_str(),
        &format!("{}-3-bcdfg", s.job),
        &format!("{}-0-bcdfa", s.job),
        &format!("{}-x-bcdfg", s.job),
        "pay-7d9f8b6c4d-x2k9p",
    ] {
        let token = s.token(&s.ns, pod, LOAD_AUDIENCE);
        assert_eq!(get(&s, "plan", &token).await.status, 403, "{pod}");
        assert_eq!(get(&s, "targets", &token).await.status, 403, "{pod}");
        assert_eq!(
            post(&s, &token, post_body(pod, false, 1)).await.status,
            403,
            "{pod}"
        );
    }
    // A token of another namespace does not see the run.
    let foreign = s.token("elsewhere", &s.pod(0), LOAD_AUDIENCE);
    assert_eq!(get(&s, "plan", &foreign).await.status, 404);
    // An unknown name or uid.
    let token = s.load_token(0);
    let other_uid = "/v1/load/load-1/ffffffff-6a3f-4c58-9e11-2f7d3a8b4c60/plan";
    assert_eq!(
        call(&s.router, Method::GET, other_uid, Some(&token), None)
            .await
            .status,
        404
    );
    let other_name = format!("/v1/load/nope/{}/plan", s.uid);
    assert_eq!(
        call(&s.router, Method::GET, &other_name, Some(&token), None)
            .await
            .status,
        404
    );
    // A run that ended is 404 on the three routes.
    s.api
        .store
        .load_end(
            &s.ns,
            DEPLOYMENT,
            &s.uid,
            &Ended {
                phase: "Succeeded",
                reason: "Completed",
                summary: "{}",
                end_ms: 1_500_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(get(&s, "plan", &token).await.status, 404);
    assert_eq!(get(&s, "targets", &token).await.status, 404);
    assert_eq!(
        post(&s, &token, post_body(&s.pod(0), true, 1)).await.status,
        404
    );
}

#[tokio::test]
async fn ldt_08_targets_follow_the_slices_and_stop_follows_the_run() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    let token = s.load_token(0);
    s.slice(&[
        ("pay-7d9f8b6c4d-zzzzz", "10.0.0.2", true),
        ("pay-7d9f8b6c4d-x2k9p", "10.0.0.1", true),
        ("pay-other-6d4c8f9b7-xxxxx", "10.0.0.3", true),
    ]);
    let body = get(&s, "targets", &token).await.json();
    assert_eq!(
        body,
        json!({"stop": false, "targets": [
            {"ip": "10.0.0.1", "pod": "pay-7d9f8b6c4d-x2k9p"},
            {"ip": "10.0.0.2", "pod": "pay-7d9f8b6c4d-zzzzz"},
        ]})
    );
    // A pod turning not ready leaves the list.
    s.slice(&[
        ("pay-7d9f8b6c4d-zzzzz", "10.0.0.2", false),
        ("pay-7d9f8b6c4d-x2k9p", "10.0.0.1", true),
    ]);
    let body = get(&s, "targets", &token).await.json();
    assert_eq!(body["targets"].as_array().unwrap().len(), 1);
    assert_eq!(body["targets"][0]["pod"], "pay-7d9f8b6c4d-x2k9p");

    // spec.stop, the controller's flag and the mode leaving mock each say `stop`.
    let mut stopped = run_info(&s.ns, &s.uid);
    stopped.stop = true;
    s.runs.apply(stopped);
    assert_eq!(get(&s, "targets", &token).await.json()["stop"], true);
    s.runs.apply(run_info(&s.ns, &s.uid));
    assert_eq!(get(&s, "targets", &token).await.json()["stop"], false);
    s.api
        .store
        .load_stop(&s.ns, DEPLOYMENT, &s.uid)
        .await
        .unwrap();
    assert_eq!(get(&s, "targets", &token).await.json()["stop"], true);
    let post_answer = post(&s, &token, post_body(&s.pod(0), false, 1)).await;
    assert_eq!(post_answer.json(), json!({"stop": true}));

    let s = setup(10_000).await;
    s.admit(PLAN).await;
    let token = s.load_token(0);
    s.slice(&[("pay-7d9f8b6c4d-x2k9p", "10.0.0.1", true)]);
    s.workloads
        .set_mock(&s.ns, "pay-w", MockSettings::default());
    assert_eq!(get(&s, "targets", &token).await.json()["stop"], true);
    assert_eq!(
        post(&s, &token, post_body(&s.pod(0), false, 1))
            .await
            .json(),
        json!({"stop": true})
    );
}

#[tokio::test]
async fn ldt_08_results_are_stored_cumulatively_per_pod() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    let (t0, t1) = (s.load_token(0), s.load_token(1));
    let answer = post(&s, &t0, post_body(&s.pod(0), false, 2)).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json(), json!({"stop": false}));
    assert_eq!(
        post(&s, &t0, post_body(&s.pod(0), false, 5)).await.status,
        200
    );
    assert_eq!(
        post(&s, &t1, post_body(&s.pod(1), true, 7)).await.status,
        200
    );
    // The same body twice changes nothing.
    assert_eq!(
        post(&s, &t1, post_body(&s.pod(1), true, 7)).await.status,
        200
    );
    let posts = s
        .api
        .store
        .load_posts(&s.ns, DEPLOYMENT, &s.uid)
        .await
        .unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[&s.pod(0)].results.endpoints[0].requests, 5);
    assert!(!posts[&s.pod(0)].final_post);
    assert_eq!(posts[&s.pod(1)].results.endpoints[0].requests, 7);
    assert!(posts[&s.pod(1)].final_post);

    // A reason is stored with the post.
    let body = serde_json::to_vec(&json!({
        "final": true, "pod": s.pod(0), "reason": "ModeLeft", "results": results(5),
    }))
    .unwrap();
    assert_eq!(post(&s, &t0, body).await.status, 200);
    let posts = s
        .api
        .store
        .load_posts(&s.ns, DEPLOYMENT, &s.uid)
        .await
        .unwrap();
    assert_eq!(posts[&s.pod(0)].reason.as_deref(), Some("ModeLeft"));
}

#[tokio::test]
async fn ldt_08_a_results_body_is_bounded_and_checked() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    let token = s.load_token(0);
    let route = s.url("results");
    // Over 256 KiB by its declared length: 413 before the body is read (none was sent).
    let answer = call(
        &s.router,
        Method::POST,
        &route,
        Some(&token),
        Some((b"{}".to_vec(), Some(MAX_POST + 1))),
    )
    .await;
    assert_eq!(answer.status, 413);
    // Exactly the bound is read (and then found malformed).
    let pod = s.pod(0);
    let ok = post_body(&pod, false, 1);
    let bad = |body: Vec<u8>| {
        let (s, token, route) = (&s, token.clone(), route.clone());
        async move {
            let length = body.len();
            call(
                &s.router,
                Method::POST,
                &route,
                Some(&token),
                Some((body, Some(length))),
            )
            .await
            .status
        }
    };
    assert_eq!(bad(b"not json".to_vec()).await, 400);
    assert_eq!(bad(b"{}".to_vec()).await, 400);
    let mut unknown: Value = serde_json::from_slice(&ok).unwrap();
    unknown["extra"] = json!(CANARY);
    assert_eq!(bad(serde_json::to_vec(&unknown).unwrap()).await, 400);
    let mut stray: Value = serde_json::from_slice(&ok).unwrap();
    stray["results"]["endpoints"][0]["address"] = json!(CANARY);
    assert_eq!(bad(serde_json::to_vec(&stray).unwrap()).await, 400);
    let mut reason: Value = serde_json::from_slice(&ok).unwrap();
    reason["reason"] = json!(CANARY);
    assert_eq!(bad(serde_json::to_vec(&reason).unwrap()).await, 400);
    // Another pod's name in the body.
    assert_eq!(bad(post_body(&s.pod(1), false, 1)).await, 400);
    // No Content-Length.
    let answer = call(
        &s.router,
        Method::POST,
        &route,
        Some(&token),
        Some((ok.clone(), None)),
    )
    .await;
    assert_eq!(answer.status, 400);
    // A body shorter than it declared.
    let answer = call(
        &s.router,
        Method::POST,
        &route,
        Some(&token),
        Some((ok.clone(), Some(ok.len() + 5))),
    )
    .await;
    assert_eq!(answer.status, 400);
    // Nothing of the refused posts was stored.
    assert!(
        s.api
            .store
            .load_posts(&s.ns, DEPLOYMENT, &s.uid)
            .await
            .unwrap()
            .is_empty()
    );
    // Methods.
    let answer = call(&s.router, Method::GET, &route, Some(&token), None).await;
    assert_eq!(answer.status, 405);
    let answer = call(&s.router, Method::POST, &s.url("plan"), Some(&token), None).await;
    assert_eq!(answer.status, 405);
}

#[tokio::test]
async fn ldt_08_the_rate_limit_comes_before_any_token_review() {
    let s = setup(1).await;
    s.admit(PLAN).await;
    let token = s.load_token(0);
    let mut statuses = Vec::new();
    for _ in 0..6 {
        statuses.push(get(&s, "plan", &token).await.status);
    }
    assert_eq!(statuses[0], 200);
    assert!(statuses.contains(&429), "{statuses:?}");
    let limited = statuses.iter().filter(|c| **c == 429).count();
    // One review served the cached answers; none was spent on a refused request.
    assert_eq!(s.load_calls.load(Ordering::SeqCst), 1);
    assert!(limited >= 1);
    let fresh = s.load_token(1);
    assert_eq!(get(&s, "plan", &fresh).await.status, 429);
    assert_eq!(s.load_calls.load(Ordering::SeqCst), 1);
    assert_eq!(s.sidecar_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ldt_08_the_review_limits_of_the_sidecar_api_apply() {
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    // The per-source TokenReview burst is 10: fresh tokens past it are 503 with no review.
    let mut statuses = Vec::new();
    for _ in 0..14 {
        statuses.push(get(&s, "plan", &s.load_token(0)).await.status);
    }
    assert!(statuses.iter().take(10).all(|c| *c == 200), "{statuses:?}");
    assert!(statuses.iter().skip(10).all(|c| *c == 503), "{statuses:?}");
    assert_eq!(s.load_calls.load(Ordering::SeqCst), 10);
}

fn logs_of(capture: &crate::webhook::tests::Capture) -> String {
    String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
}

async fn keep_valkey_values(s: &Setup) -> String {
    let mut c = redis::Client::open(valkey_url())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let key = format!("mimic:v1:{{{}/{DEPLOYMENT}}}:load:{}", s.ns, s.uid);
    let all: Vec<Vec<u8>> = redis::cmd("HGETALL")
        .arg(key)
        .query_async(&mut c)
        .await
        .unwrap();
    all.iter()
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn ldt_14_no_canary_and_no_ticket_in_logs_series_answers_or_stored_values() {
    let (capture, _guard) = crate::webhook::tests::capture_logs();
    let s = setup(10_000).await;
    s.admit(PLAN).await;
    s.slice(&[("pay-7d9f8b6c4d-x2k9p", "10.0.0.1", true)]);
    let token = s.load_token(0);

    // Honest calls, and refused ones that carry canaries in every place a client controls.
    let plan = get(&s, "plan", &token).await;
    let targets = get(&s, "targets", &token).await;
    let ok = post(&s, &token, post_body(&s.pod(0), true, 4)).await;
    let mut hostile: Value = serde_json::from_slice(&post_body(&s.pod(0), false, 1)).unwrap();
    hostile["pod"] = json!(CANARY);
    hostile["reason"] = json!(CANARY);
    let hostile = serde_json::to_vec(&hostile).unwrap();
    let length = hostile.len();
    let refused = call(
        &s.router,
        Method::POST,
        &s.url("results"),
        Some(&token),
        Some((hostile, Some(length))),
    )
    .await;
    assert_eq!(refused.status, 400);
    let canary_token = s.token(&s.ns, CANARY, LOAD_AUDIENCE);
    let forbidden = get(&s, "plan", &canary_token).await;
    assert_eq!(forbidden.status, 403);
    let by_name = call(
        &s.router,
        Method::GET,
        &format!("/v1/load/{CANARY}/{}/plan", s.uid),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(by_name.status, 404);

    // The plan answer is the only one that holds the ticket.
    assert!(String::from_utf8_lossy(&plan.bytes).contains(TICKET));
    for answer in [&targets, &ok, &refused, &forbidden, &by_name] {
        let text = String::from_utf8_lossy(&answer.bytes);
        assert!(!text.contains(TICKET) && !text.contains(CANARY), "{text}");
    }
    // Ended: the summary is built from the typed results and holds neither.
    let summary = mimic_core::load::summary::summary(
        &mimic_core::load::plan::Plan::default(),
        &s.api
            .store
            .load_posts(&s.ns, DEPLOYMENT, &s.uid)
            .await
            .unwrap()
            .values()
            .map(|p| p.results.clone())
            .collect::<Vec<_>>(),
        1000,
    );
    let summary = serde_json::to_string(&summary).unwrap();
    s.api
        .store
        .load_end(
            &s.ns,
            DEPLOYMENT,
            &s.uid,
            &Ended {
                phase: "Succeeded",
                reason: "Completed",
                summary: &summary,
                end_ms: 1_500_000,
            },
        )
        .await
        .unwrap();

    let logs = logs_of(&capture);
    let series = s.metrics.encode();
    let stored = keep_valkey_values(&s).await;
    for (what, text) in [("logs", &logs), ("series", &series), ("valkey", &stored)] {
        assert!(!text.contains(CANARY), "a canary is in the {what}");
        assert!(!text.contains(TICKET), "the ticket is in the {what}");
        assert!(
            !text.contains("eyJ0aWNrZXQiOjF9"),
            "the ticket payload is in the {what}"
        );
    }
    // The routes were logged and counted, without the run's name in a label.
    assert!(logs.contains("/v1/load/plan"), "{logs}");
    assert!(series.contains(r#"endpoint="/v1/load/plan""#), "{series}");
    // The bearer tokens are not in the logs either.
    assert!(!logs.contains(&token));
}
