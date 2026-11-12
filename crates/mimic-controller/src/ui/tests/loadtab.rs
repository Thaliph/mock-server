//! EXP-13 to EXP-15: the Load tests tab (spec §7.5, §12.17). The shell tests need no Valkey;
//! EXP-14 reads stored summaries and needs the Valkey of `MIMIC_TEST_VALKEY_URL`.

use super::*;
use crate::resources::ResourceCache;
use crate::store::load::{Admit, AdmitLimits, Ended};

const UID: &str = "55555555-5555-4555-8555-555555555555";
const LIVE: &str = "66666666-6666-4666-8666-666666666666";

const SUMMARY: &str = r#"{"total":{"requests":40,"missed":2,"statuses":{"2xx":38,"5xx":2},"errors":{"timeout":3,"reset":1},"rps":20,"latencyMicros":{"p50":800,"p90":1900,"p99":4800,"max":8000}},"endpoints":[{"method":"GET","port":8080,"template":"/a","requests":25,"missed":1,"statuses":{"2xx":25},"errors":{"timeout":3},"rps":12,"latencyMicros":{"p50":700,"p90":1500,"p99":4000,"max":7000}},{"method":"PUT","port":8080,"template":"/b/{id}","requests":15,"missed":1,"statuses":{"2xx":13,"5xx":2},"errors":{"reset":1},"rps":8,"latencyMicros":{"p50":900,"p90":2100,"p99":4800,"max":8000}}]}"#;

fn shell(enabled: Option<bool>) -> (Router, Metrics) {
    let mut metrics = Metrics::new("controller", "test");
    let ui = UiMetrics::register(metrics.registry_mut());
    let cache = ResourceCache::new();
    if let Some(enabled) = enabled {
        cache.set_load_enabled(enabled);
    }
    (
        router_with(store("redis://127.0.0.1:1"), ui, cache),
        metrics,
    )
}

fn mimic_js() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/mimic.js")).unwrap()
}

#[tokio::test]
async fn exp_13_features_say_whether_load_tests_are_enabled() {
    for (set, want) in [(None, false), (Some(false), false), (Some(true), true)] {
        let (app, _) = shell(set);
        let answer = fetch(&app, "/v1/features").await;
        assert_eq!(answer.status, 200, "{set:?}");
        assert_eq!(answer.headers["content-type"], "application/json");
        assert_eq!(answer.headers["x-content-type-options"], "nosniff");
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(body, serde_json::json!({"loadTest": {"enabled": want}}));
    }
}

#[tokio::test]
async fn exp_13_the_run_page_validates_the_uid_and_the_names() {
    let (app, _) = shell(None);
    let ui = fetch(&app, "/ui/").await;
    let page = fetch(&app, &format!("/ui/workloads/shop/front/loadtests/{UID}")).await;
    assert_eq!(page.status, 200);
    assert_eq!(page.headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(page.body, ui.body, "not the static page");
    for path in [
        "/ui/workloads/shop/front/loadtests/x",
        "/ui/workloads/shop/front/loadtests/55555555-5555-4555-8555-55555555555G",
        "/ui/workloads/shop/front/loadtests/55555555-5555-4555-8555-55555555555A",
        "/ui/workloads/Bad_Ns/front/loadtests/55555555-5555-4555-8555-555555555555",
        "/ui/workloads/shop/Bad_Dep/loadtests/55555555-5555-4555-8555-555555555555",
    ] {
        assert_eq!(fetch(&app, path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn exp_13_head_405_and_403_hold_on_the_new_routes() {
    let (app, _) = shell(None);
    let local = Some("localhost:8080");
    for path in [
        "/v1/features".to_string(),
        format!("/ui/workloads/shop/front/loadtests/{UID}"),
    ] {
        let get = fetch(&app, &path).await;
        let head = send(&app, Method::HEAD, &path, "127.0.0.1", local).await;
        assert_eq!(head.status, get.status, "{path}");
        assert!(head.body.is_empty(), "{path}");
        let other = send(&app, Method::POST, &path, "127.0.0.1", local).await;
        assert_eq!(other.status, 405, "{path}");
        assert_eq!(other.headers["allow"], "GET, HEAD", "{path}");
        for (peer, host) in [("10.1.2.3", local), ("127.0.0.1", Some("evil.example"))] {
            assert_eq!(
                send(&app, Method::GET, &path, peer, host).await.status,
                403,
                "{path} {peer}"
            );
        }
        for header in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
            "x-frame-options",
            "cache-control",
        ] {
            assert!(get.headers.contains_key(header), "{path}: {header}");
        }
    }
}

#[tokio::test]
async fn exp_13_the_new_routes_count_by_pattern_without_names() {
    let (app, metrics) = shell(Some(true));
    fetch(&app, "/v1/features").await;
    fetch(&app, &format!("/ui/workloads/shop/front/loadtests/{UID}")).await;
    fetch(&app, "/ui/workloads/shop/front/loadtests/x").await;
    for (route, code) in [
        ("/v1/features", 200),
        (
            "/ui/workloads/{namespace}/{deployment}/loadtests/{uid}",
            200,
        ),
        (
            "/ui/workloads/{namespace}/{deployment}/loadtests/{uid}",
            400,
        ),
    ] {
        assert_eq!(requests(&metrics, route, code), Some(1), "{route} {code}");
    }
    let text = metrics.encode();
    for name in ["shop", "front", UID] {
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("mimic_ui_") && l.contains(name)),
            "{name} is in a series"
        );
    }
}

async fn admit(store: &Arc<Store>, ns: &str, uid: &str, name: &str, ended: bool) {
    let spec = format!(
        r#"{{"name":"{name}","deployment":"pay","credentials":{{"secretName":"payment-load","fields":[{{"field":"header/authorization"}}]}}}}"#
    );
    store
        .load_admit(&Admit {
            namespace: ns,
            deployment: "pay",
            uid,
            spec: &spec,
            plan: "{}",
            ticket: "v1.secret-ticket.signature",
            now_ms: 1_000_000,
            deadline_ms: 1_600_000,
            reserve: 1 << 10,
            limits: AdmitLimits {
                max_runs: 1000,
                max_namespace_runs: 1000,
                budget: 1 << 40,
            },
        })
        .await
        .expect("admitted");
    if ended {
        store
            .load_end(
                ns,
                "pay",
                uid,
                &Ended {
                    phase: "Succeeded",
                    reason: "Completed",
                    summary: SUMMARY,
                    end_ms: 1_100_000,
                },
            )
            .await
            .expect("ended");
    }
}

#[tokio::test]
async fn exp_14_the_routes_give_the_tab_the_members_it_reads() {
    let store = store(&valkey_url());
    let ns = unique("exp14");
    admit(&store, &ns, UID, "nightly", true).await;
    admit(&store, &ns, LIVE, "burst", false).await;
    let list = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests")).await;
    assert_eq!(list.status, 200);
    let body = list.json();
    let runs = body["loadtests"].as_array().expect("a list");
    assert_eq!(runs.len(), 2, "the list holds two runs");
    let ended = runs
        .iter()
        .find(|r| r["uid"] == UID)
        .expect("the ended run");
    for member in ["name", "phase", "reason", "start", "end"] {
        assert!(ended[member].is_string(), "{member}: {ended}");
    }
    assert_eq!(ended["total"]["requests"], 40);
    assert_eq!(ended["total"]["rps"], 20);
    assert_eq!(ended["total"]["latencyMicros"]["p99"], 4800);
    let run = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests/{UID}")).await;
    assert_eq!(run.status, 200);
    let body = run.json();
    assert_eq!(body["summary"]["total"]["errors"]["timeout"], 3);
    assert_eq!(body["summary"]["total"]["errors"]["reset"], 1);
    let endpoints = body["summary"]["endpoints"].as_array().expect("endpoints");
    assert_eq!(endpoints.len(), 2);
    for e in endpoints {
        for member in ["method", "port", "template", "requests", "missed", "rps"] {
            assert!(!e[member].is_null(), "{member}: {e}");
        }
        assert!(e["statuses"].is_object() && e["errors"].is_object());
        for q in ["p50", "p90", "p99", "max"] {
            assert!(e["latencyMicros"][q].is_u64(), "{q}: {e}");
        }
    }
    for text in [list.json().to_string(), body.to_string()] {
        for secret in [
            "payment-load",
            "header/authorization",
            "secret-ticket",
            "credentials",
        ] {
            assert!(
                !text.contains(secret),
                "an answer holds a credential member"
            );
        }
    }
}

#[test]
fn exp_15_the_load_tests_tab_reads_the_routes_and_has_its_texts() {
    let js = mimic_js();
    assert!(
        !js.contains("not available here yet"),
        "the placeholder is still there"
    );
    for needle in [
        "/v1/features",
        "/v1/workloads\"",
        "/v1/resources",
        "\"/loadtests\"",
        "Load testing is disabled",
        "No load test runs",
        "No such run",
        "parts[4] === \"loadtests\"",
        "parts[1] === \"workloads\"",
        "/loadtests/",
        "enc(",
        "encodeURIComponent",
        "latencyMicros",
    ] {
        assert!(js.contains(needle), "mimic.js lacks {needle}");
    }
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "Function(",
        "http://",
        "https://",
    ] {
        assert!(!js.contains(forbidden), "{forbidden}");
    }
}
