//! MSH-19 to MSH-21, MSH-23 to MSH-25: `GET /v1/workloads/<deployment>/mesh` (spec §7.2),
//! in-process, with a fake `TokenReview` and hand-fed objects.

use std::io::Read;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, Request};
use http_body_util::BodyExt;
use mimic_core::Mode;
use mimic_core::mesh::view::{Limits, View, etag};
use mimic_core::mock::MockSettings;
use mimic_runtime::metrics::Metrics;
use rcgen::{KeyPair, PublicKeyData};
use serde_json::json;
use tower::ServiceExt;

use super::tests::{Reviews, api_server};
use super::*;
use crate::mesh::tests::*;
use crate::mesh::{Event, Hub, MeshConfig, MeshMetrics};
use crate::store::{StoreConfig, StoreMetrics};
use crate::tls_ca::{Seed, SeedSlot};

struct Setup {
    router: Router,
    hub: Arc<Hub>,
    workloads: Arc<WorkloadCache>,
    reviews: Reviews,
    metrics: MeshMetrics,
}

struct Resp {
    status: u16,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn etag(&self) -> String {
        self.headers["etag"].to_str().unwrap().to_string()
    }
}

fn config(watch_timeout_ms: u64, max_watches: usize) -> MeshConfig {
    MeshConfig {
        watch_timeout: Duration::from_millis(watch_timeout_ms),
        debounce: Duration::from_millis(50),
        max_watches,
        ..MeshConfig::default()
    }
}

/// `shop` has `cart` and `pay` with endpoints; `risk` has `scoring`; the watches are synced and
/// published; `run` is the debouncer.
fn setup_with(mesh: Option<MeshConfig>, rate: u32, run: bool) -> Setup {
    build(mesh, rate, run, true)
}

fn build(mesh: Option<MeshConfig>, rate: u32, run: bool, feed: bool) -> Setup {
    let (client, reviews) = api_server();
    let mut metrics = Metrics::new("controller", "test");
    let api_metrics = ApiMetrics::register(metrics.registry_mut());
    let mesh_metrics = MeshMetrics::default();
    let hub = mesh.map(|c| Hub::new(c, mesh_metrics.clone()));
    let workloads = Arc::new(WorkloadCache::synced_with(&[("shop", "w", "cart")]));
    let seed = SeedSlot::new();
    seed.set(Seed::generate(SystemTime::now()));
    let api = Arc::new(SidecarApi {
        seed: Arc::new(seed),
        reviewer: Arc::new(KubeReviewer::new(Arc::new(OnceCell::new_with(Some(
            client,
        ))))),
        metrics: api_metrics.clone(),
        limiter: ReviewLimiter::default(),
        rate: RateLimiter::new(rate),
        reviews: ReviewCache::default(),
        budget: BodyBudget::new(1 << 30),
        store: Store::new(
            redis::Client::open("redis://127.0.0.1:1").expect("a valid URL"),
            StoreConfig {
                ttl: Duration::from_secs(86_400),
                limits: mimic_core::discovery::report::Limits {
                    max_literals: 32,
                    max_endpoints: 256,
                    max_authorities: 64,
                    sample_budget: 1 << 20,
                },
                max_deployments: 50,
            },
            StoreMetrics::register(metrics.registry_mut()),
        ),
        workloads: workloads.clone(),
        max_report: 1 << 20,
        max_push_endpoints: 256,
        max_egress: 4 << 20,
        egress: egress::Cache::new(32 << 20, &api_metrics),
        egress_logged: egress::Logged::default(),
        load: None,
        mesh: hub.clone(),
    });
    if let Some(hub) = hub.as_ref().filter(|_| feed) {
        synced(hub);
        put_service(hub, svc("shop", "cart", "10.96.0.1"));
        put_service(hub, svc("shop", "pay", "10.96.0.2"));
        put_slice(hub, slice("shop", "cart", "cart-a", &["10.244.0.1"]));
        put_service(hub, svc("risk", "scoring", "10.96.5.5"));
        put_slice(hub, slice("risk", "scoring", "scoring-a", &["10.244.7.7"]));
        put_deployment(hub, "shop", "cart");
        hub.publish_now();
        if run {
            tokio::spawn(hub.clone().run());
        }
    }
    Setup {
        router: router(api),
        hub: hub.unwrap_or_else(|| Hub::new(MeshConfig::default(), mesh_metrics.clone())),
        workloads,
        reviews,
        metrics: mesh_metrics,
    }
}

fn setup() -> Setup {
    setup_with(Some(config(400, 100)), 10_000, false)
}

fn token(ns: &str, deployment: &str) -> String {
    format!("ns|{ns}|{deployment}-7d9f8b6c4d-x2k9p|t")
}

async fn get_at(router: &Router, uri: &str, token: Option<&str>, headers: &[(&str, &str)]) -> Resp {
    let mut request = Request::builder().method(Method::GET).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(Peer("10.1.1.1".parse().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Resp {
        status: parts.status.as_u16(),
        headers: parts.headers,
        bytes: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

async fn get(s: &Setup, deployment: &str, ns: &str, headers: &[(&str, &str)]) -> Resp {
    let uri = format!("/v1/workloads/{deployment}/mesh?format=1");
    get_at(&s.router, &uri, Some(&token(ns, deployment)), headers).await
}

fn view_of(r: &Resp) -> View {
    View::from_json(&r.text(), &Limits::default()).expect("a view")
}

#[tokio::test]
async fn msh_19_refusals_have_the_status_of_the_other_routes() {
    let s = setup();
    let uri = "/v1/workloads/cart/mesh?format=1";
    let good = token("shop", "cart");
    assert_eq!(get_at(&s.router, uri, None, &[]).await.status, 401);
    assert_eq!(get_at(&s.router, uri, Some("nope"), &[]).await.status, 401);
    assert_eq!(
        get_at(&s.router, uri, Some("wrong-audience"), &[])
            .await
            .status,
        401
    );
    // A pod of deployment `web` asks for the view of `cart`, or has no bound pod.
    assert_eq!(
        get_at(&s.router, uri, Some(&token("shop", "web")), &[])
            .await
            .status,
        403
    );
    assert_eq!(
        get_at(&s.router, uri, Some("no-claim"), &[]).await.status,
        403
    );
    for bad in [
        "/v1/workloads/Cart/mesh?format=1",
        "/v1/workloads/cart/mesh",
        "/v1/workloads/cart/mesh?format=2",
        "/v1/workloads/cart/mesh?format=abc",
    ] {
        assert_eq!(
            get_at(&s.router, bad, Some(&good), &[]).await.status,
            400,
            "{bad}"
        );
    }
    let mut post = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    post.extensions_mut()
        .insert(Peer("10.1.1.1".parse().unwrap()));
    let response = s.router.clone().oneshot(post).await.unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(response.headers()["allow"], "GET");
    assert_eq!(get_at(&s.router, uri, Some(&good), &[]).await.status, 200);
}

#[tokio::test]
async fn msh_19_over_the_rate_is_429_with_no_token_review() {
    let s = setup_with(Some(config(400, 100)), 1, false);
    let uri = "/v1/workloads/cart/mesh?format=1";
    let good = token("shop", "cart");
    let mut statuses = Vec::new();
    for _ in 0..6 {
        statuses.push(get_at(&s.router, uri, Some(&good), &[]).await.status);
    }
    assert_eq!(
        statuses.iter().filter(|c| **c == 429).count(),
        4,
        "{statuses:?}"
    );
    // One review for the token: the refused requests cost none.
    assert_eq!(s.reviews.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn msh_19_until_the_watches_are_synced_the_answer_is_503_with_retry_after() {
    let s = build(Some(config(400, 100)), 10_000, false, false);
    let r = get(&s, "cart", "shop", &[]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.headers["retry-after"], "1");
    // A relist in progress does not make a synced hub unsynced.
    let s = setup();
    s.hub.feed_services(Event::Init);
    assert_eq!(get(&s, "cart", "shop", &[]).await.status, 200);
}

#[tokio::test]
async fn msh_19_the_view_is_the_one_of_the_tokens_namespace_whatever_the_path_says() {
    let s = setup();
    // `cart` of `shop` sees `shop` and nothing of `risk` (no grant, no declaration).
    let view = view_of(&get(&s, "cart", "shop", &[]).await);
    assert_eq!(
        view.services.keys().collect::<Vec<_>>(),
        ["shop/cart", "shop/pay"]
    );
    // `pay` exists only as a name here: a pod of namespace `risk` naming it gets `risk`'s view.
    let other = get(&s, "pay", "risk", &[]).await;
    assert_eq!(other.status, 200);
    assert_eq!(
        view_of(&other).services.keys().collect::<Vec<_>>(),
        ["risk/scoring"]
    );
    assert!(!other.text().contains("10.244.0.1") && !other.text().contains("shop/"));
    assert!(
        !get(&s, "cart", "shop", &[])
            .await
            .text()
            .contains("10.244.7.7")
    );
}

#[tokio::test]
async fn msh_19_a_deployment_with_another_winning_config_gets_other_bytes() {
    let s = setup();
    let plain = get(&s, "pay", "shop", &[]).await;
    put_config(
        &s.hub,
        crate::mesh::tests::config("shop", "pay-cfg", "pay", &[]),
    );
    s.hub.publish_now();
    let configured = get(&s, "pay", "shop", &[]).await;
    assert_ne!(plain.etag(), configured.etag());
    assert!(configured.text().contains("mimicconfig/shop/pay-cfg"));
    // `cart` has no config: its view is not changed by the config of `pay`.
    assert!(
        !get(&s, "cart", "shop", &[])
            .await
            .text()
            .contains("pay-cfg")
    );
}

#[tokio::test]
async fn msh_20_the_answer_is_the_built_view_with_its_etag_and_gzip_on_request() {
    let s = setup();
    let plain = get(&s, "cart", "shop", &[]).await;
    assert_eq!(plain.status, 200);
    assert_eq!(plain.headers["content-type"], "application/json");
    assert_eq!(plain.headers["cache-control"], "no-store");
    assert!(!plain.headers.contains_key("content-encoding"));
    assert_eq!(plain.etag(), etag(&plain.text()));
    let expected = mimic_core::mesh::view::build(
        &s.hub.published().snapshot,
        "shop",
        "cart",
        Mode::None,
        &Limits::default(),
    );
    assert_eq!(plain.text(), expected.json);
    assert_eq!(view_of(&plain).mode, "none");

    let zipped = get(&s, "cart", "shop", &[("accept-encoding", "br, gzip;q=0.8")]).await;
    assert_eq!(zipped.headers["content-encoding"], "gzip");
    assert_eq!(zipped.etag(), plain.etag());
    let mut inflated = String::new();
    flate2::read::GzDecoder::new(&zipped.bytes[..])
        .read_to_string(&mut inflated)
        .unwrap();
    assert_eq!(inflated, plain.text());
    let refused = get(&s, "cart", "shop", &[("accept-encoding", "gzip;q=0")]).await;
    assert!(!refused.headers.contains_key("content-encoding"));
}

#[tokio::test]
async fn msh_20_a_change_of_mode_is_served_at_once_with_the_new_etag() {
    let s = setup();
    let before = get(&s, "cart", "shop", &[]).await;
    s.workloads.set_mock(
        "shop",
        "w",
        MockSettings {
            mock: true,
            ..MockSettings::default()
        },
    );
    let held = [("if-none-match", before.etag())];
    let after = get(&s, "cart", "shop", &[("if-none-match", &held[0].1)]).await;
    assert_eq!(after.status, 200);
    assert!(after.text().contains(r#""mode":"mock""#));
    assert_ne!(after.etag(), before.etag());
}

#[tokio::test]
async fn msh_21_a_stale_etag_gets_the_view_at_once_and_the_current_one_is_held_until_304() {
    let s = setup();
    let first = get(&s, "cart", "shop", &[]).await;
    let started = Instant::now();
    let stale = get(&s, "cart", "shop", &[("if-none-match", "\"1-stale\"")]).await;
    assert_eq!(stale.status, 200);
    assert!(started.elapsed() < Duration::from_millis(200));

    let started = Instant::now();
    let held = get(&s, "cart", "shop", &[("if-none-match", &first.etag())]).await;
    assert_eq!(held.status, 304);
    assert_eq!(held.etag(), first.etag());
    assert!(held.bytes.is_empty());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(390) && waited < Duration::from_millis(900),
        "{waited:?}"
    );
    // A list holds; a weak validator never matches (as for the egress pull).
    let list = format!("\"1-other\", {}", first.etag());
    let started = Instant::now();
    assert_eq!(
        get(&s, "cart", "shop", &[("if-none-match", &list)])
            .await
            .status,
        304
    );
    assert!(started.elapsed() >= Duration::from_millis(390));
    let weak = format!("W/{}", first.etag());
    assert_eq!(
        get(&s, "cart", "shop", &[("if-none-match", &weak)])
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn msh_21_a_change_wakes_a_held_request_within_the_debounce_plus_500_ms() {
    let s = setup_with(Some(config(5_000, 100)), 10_000, true);
    let first = get(&s, "cart", "shop", &[]).await;
    let held = tokio::spawn({
        let (router, etag) = (s.router.clone(), first.etag());
        async move {
            get_at(
                &router,
                "/v1/workloads/cart/mesh?format=1",
                Some(&token("shop", "cart")),
                &[("if-none-match", &etag)],
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(s.metrics.watches.get(), 1);
    let changed = Instant::now();
    put_slice(
        &s.hub,
        slice("shop", "cart", "cart-a", &["10.244.0.1", "10.244.0.99"]),
    );
    let answer = held.await.unwrap();
    assert_eq!(answer.status, 200);
    assert!(
        changed.elapsed() < Duration::from_millis(550),
        "{:?}",
        changed.elapsed()
    );
    assert!(answer.text().contains("10.244.0.99"));
    assert_eq!(s.metrics.watches.get(), 0);
}

#[tokio::test]
async fn msh_21_a_change_of_another_namespace_does_not_wake_a_held_request() {
    let s = setup_with(Some(config(600, 100)), 10_000, true);
    let first = get(&s, "cart", "shop", &[]).await;
    let held = tokio::spawn({
        let (router, etag) = (s.router.clone(), first.etag());
        async move {
            get_at(
                &router,
                "/v1/workloads/cart/mesh?format=1",
                Some(&token("shop", "cart")),
                &[("if-none-match", &etag)],
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    put_slice(
        &s.hub,
        slice(
            "risk",
            "scoring",
            "scoring-a",
            &["10.244.7.7", "10.244.7.8"],
        ),
    );
    put_service(&s.hub, svc("other", "x", "10.96.9.9"));
    let answer = held.await.unwrap();
    assert_eq!(answer.status, 304);
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn msh_23_a_full_watch_budget_answers_503_and_pushes_and_certificates_are_still_served() {
    let s = setup_with(Some(config(700, 2)), 10_000, false);
    let first = get(&s, "cart", "shop", &[]).await;
    let hold = |s: &Setup| {
        let (router, etag) = (s.router.clone(), first.etag());
        tokio::spawn(async move {
            get_at(
                &router,
                "/v1/workloads/cart/mesh?format=1",
                Some(&token("shop", "cart")),
                &[("if-none-match", &etag)],
            )
            .await
        })
    };
    let (a, b) = (hold(&s), hold(&s));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(s.metrics.watches.get(), 2);

    let third = get(&s, "cart", "shop", &[("if-none-match", &first.etag())]).await;
    assert_eq!(third.status, 503);
    assert_eq!(third.headers["retry-after"], "1");
    // An answer that is not held takes no share.
    assert_eq!(
        get(&s, "cart", "shop", &[("if-none-match", "\"1-stale\"")])
            .await
            .status,
        200
    );
    // A certificate request goes through while the budget is full.
    let key = KeyPair::generate().unwrap();
    let body = serde_json::to_vec(&json!({
        "deployment": "cart",
        "public_key": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key.subject_public_key_info()),
    }))
    .unwrap();
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/tls/intermediate")
        .header("authorization", format!("Bearer {}", token("shop", "cart")))
        .body(Body::from(body))
        .unwrap();
    request
        .extensions_mut()
        .insert(Peer("10.1.1.2".parse().unwrap()));
    assert_eq!(
        s.router.clone().oneshot(request).await.unwrap().status(),
        200
    );

    assert_eq!(
        (a.await.unwrap().status, b.await.unwrap().status),
        (304, 304)
    );
    assert_eq!(s.metrics.watches.get(), 0);
    // The share is free again.
    assert_eq!(
        get(&s, "cart", "shop", &[("if-none-match", &first.etag())])
            .await
            .status,
        304
    );
}

#[tokio::test]
async fn msh_24_with_the_mesh_off_the_route_answers_404_without_a_token_review() {
    let s = setup_with(None, 10_000, false);
    for token in [Some(token("shop", "cart")), Some("nope".to_string()), None] {
        let r = get_at(
            &s.router,
            "/v1/workloads/cart/mesh?format=1",
            token.as_deref(),
            &[],
        )
        .await;
        assert_eq!(r.status, 404);
    }
    assert!(s.reviews.lock().unwrap().is_empty());
}

#[tokio::test]
async fn msh_25_a_grant_wakes_a_held_request_with_the_destination_and_its_deletion_removes_it() {
    let s = setup_with(Some(config(5_000, 100)), 10_000, true);
    put_config(
        &s.hub,
        crate::mesh::tests::config("shop", "c", "cart", &[("risk", "scoring")]),
    );
    s.hub.publish_now();
    let first = get(&s, "cart", "shop", &[]).await;
    assert!(!first.text().contains("10.244.7.7"));
    let wait = |etag: String| {
        let router = s.router.clone();
        tokio::spawn(async move {
            get_at(
                &router,
                "/v1/workloads/cart/mesh?format=1",
                Some(&token("shop", "cart")),
                &[("if-none-match", &etag)],
            )
            .await
        })
    };
    let held = wait(first.etag());
    tokio::time::sleep(Duration::from_millis(100)).await;
    put_grant(&s.hub, "g", grant("risk", "shop"));
    let granted = held.await.unwrap();
    assert_eq!(granted.status, 200);
    assert!(granted.text().contains("10.244.7.7"));

    let held = wait(granted.etag());
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.hub.feed_grants(Event::Delete(key("risk", "g")));
    let revoked = held.await.unwrap();
    assert_eq!(revoked.status, 200);
    assert!(!revoked.text().contains("10.244.7.7") && !revoked.text().contains("risk/"));
}
