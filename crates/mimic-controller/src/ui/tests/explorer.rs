//! EXP-03 to EXP-06: the explorer shell and the Workloads tab (spec §7.5, §12.16). No Valkey:
//! the resource routes and the pages read the watch caches and static bytes only, so the store
//! points at a closed port.

use kube::api::DynamicObject;
use serde_json::json;

use super::*;
use crate::resources::{Resource, ResourceCache};

const TABS: [&str; 4] = ["/ui/map", "/ui/", "/ui/endpoints", "/ui/loadtests"];
const HOSTILE: &str = "<img src=x onerror=alert(1)>";

fn held(value: Value) -> Resource {
    let object: DynamicObject = serde_json::from_value(value).expect("an object");
    let kind = object.types.as_ref().expect("a kind").kind.clone();
    Resource::from_object(&kind, &object).expect("held")
}

fn cache() -> Arc<ResourceCache> {
    let cache = ResourceCache::new();
    cache.apply(held(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
        "metadata": {
            "name": "front", "namespace": "shop",
            "labels": {"team": HOSTILE},
            "annotations": {"note": HOSTILE,
                "kubectl.kubernetes.io/last-applied-configuration": "{}"},
            "managedFields": [{"manager": "kubectl"}]
        },
        "spec": {"deployment": "front", "mode": "mock"},
        "status": {"conditions": [{"type": "Ready", "status": "True", "message": HOSTILE}]}
    })));
    cache.apply(held(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicLoadTest",
        "metadata": {"name": "burst", "namespace": "shop"},
        "spec": {"deployment": "front", "duration": "5m",
            "credentials": {"secretName": "payment-load",
                "fields": [{"field": "header/authorization", "key": "token"}]}},
        "status": {"phase": "Succeeded"}
    })));
    cache.apply(held(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
        "metadata": {"name": "aaa", "namespace": "shop"},
        "spec": {"deployment": "front"}
    })));
    cache.apply(held(json!({
        "apiVersion": "mimic.platform.io/v1alpha1", "kind": "MimicWorkload",
        "metadata": {"name": "w", "namespace": "bank"},
        "spec": {"deployment": "pay"}
    })));
    cache
}

fn explorer() -> (Router, Metrics) {
    let mut metrics = Metrics::new("controller", "test");
    let ui = UiMetrics::register(metrics.registry_mut());
    let store = store("redis://127.0.0.1:1");
    (router_with(store, ui, cache()), metrics)
}

fn json_of(answer: &Full) -> Value {
    serde_json::from_slice(&answer.body).expect("a JSON body")
}

const NEW_ROUTES: [&str; 6] = [
    "/v1/resources",
    "/v1/workloads/shop/front/resources",
    "/ui/map",
    "/ui/endpoints",
    "/ui/loadtests",
    "/ui/workloads/shop/front/resources/MimicWorkload/front",
];

#[tokio::test]
async fn exp_03_resources_of_a_deployment_are_sorted_with_yaml_and_conditions() {
    let (app, _) = explorer();
    let answer = fetch(&app, "/v1/workloads/shop/front/resources").await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.headers["content-type"], "application/json");
    let body = json_of(&answer);
    let rows: Vec<(&str, &str)> = body["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["kind"].as_str().unwrap(), r["name"].as_str().unwrap()))
        .collect();
    assert_eq!(
        rows,
        [
            ("MimicLoadTest", "burst"),
            ("MimicWorkload", "aaa"),
            ("MimicWorkload", "front")
        ]
    );
    let front = &body["resources"][2];
    assert_eq!(front["namespace"], "shop");
    assert_eq!(
        front["spec"],
        json!({"deployment": "front", "mode": "mock"})
    );
    assert!(front["yaml"].as_str().unwrap().contains("mode: \"mock\"\n"));
    assert_eq!(front["conditions"][0]["type"], "Ready");
    assert_eq!(body["resources"][0]["phase"], "Succeeded");
    assert!(front.get("phase").is_none());
    let text = String::from_utf8(answer.body.clone()).unwrap();
    assert!(!text.contains("managedFields") && !text.contains("last-applied"));
}

#[tokio::test]
async fn exp_03_a_deployment_without_resource_is_an_empty_list_and_bad_names_are_400() {
    let (app, _) = explorer();
    let none = fetch(&app, "/v1/workloads/shop/other/resources").await;
    assert_eq!(none.status, 200);
    assert_eq!(json_of(&none), json!({"resources": []}));
    for path in [
        "/v1/workloads/Bad_Ns/front/resources",
        "/v1/workloads/shop/Bad_Dep/resources",
    ] {
        assert_eq!(fetch(&app, path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn exp_03_the_overview_lists_every_named_deployment_with_its_mode() {
    let (app, _) = explorer();
    let answer = fetch(&app, "/v1/resources").await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        json_of(&answer),
        json!({"workloads": [
            {"namespace": "bank", "deployment": "pay", "mode": "discovery",
             "resources": [{"kind": "MimicWorkload", "name": "w"}]},
            {"namespace": "shop", "deployment": "front", "mode": "mock",
             "resources": [
                {"kind": "MimicLoadTest", "name": "burst"},
                {"kind": "MimicWorkload", "name": "aaa"},
                {"kind": "MimicWorkload", "name": "front"}]}
        ]})
    );
}

#[tokio::test]
async fn exp_03_the_resource_routes_answer_with_valkey_stopped() {
    // The store of `explorer` points at a closed port.
    let (app, _) = explorer();
    for path in ["/v1/resources", "/v1/workloads/shop/front/resources"] {
        assert_eq!(fetch(&app, path).await.status, 200, "{path}");
    }
}

#[tokio::test]
async fn exp_04_every_tab_page_holds_the_four_tabs_in_order() {
    let (app, _) = explorer();
    for path in [
        "/ui/",
        "/ui/map",
        "/ui/endpoints",
        "/ui/loadtests",
        "/ui/workloads/shop/front",
        "/ui/workloads/shop/front/resources/MimicWorkload/front",
    ] {
        let page = fetch(&app, path).await;
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(
            page.headers["content-type"], "text/html; charset=utf-8",
            "{path}"
        );
        let html = String::from_utf8(page.body).unwrap();
        let mut from = 0;
        for tab in TABS {
            let link = format!("href=\"{tab}\"");
            let at = html[from..]
                .find(&link)
                .unwrap_or_else(|| panic!("{path}: no tab {tab} after offset {from}"));
            from += at + link.len();
        }
    }
}

#[tokio::test]
async fn exp_04_the_new_pages_follow_the_ui_09_rules() {
    let (app, _) = explorer();
    for path in [
        "/ui/map",
        "/ui/endpoints",
        "/ui/loadtests",
        "/ui/workloads/shop/front/resources/MimicLoadTest/burst",
    ] {
        let html = String::from_utf8(fetch(&app, path).await.body).unwrap();
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "{path}"
        );
        assert!(
            !html.contains("style=") && !html.contains("<style"),
            "{path}"
        );
        assert!(!html.contains("innerHTML"), "{path}");
        for tag in html.split('<').skip(1) {
            let tag = tag.split('>').next().unwrap();
            for attr in tag.split_whitespace().skip(1) {
                assert!(
                    !attr.to_ascii_lowercase().starts_with("on"),
                    "{path}: {attr}"
                );
            }
            if tag.starts_with("script") || tag.starts_with("link") {
                assert!(tag.contains("=\"/ui/assets/"), "{path}: {tag}");
            }
        }
    }
}

#[tokio::test]
async fn exp_04_the_resource_page_validates_kind_and_name() {
    let (app, _) = explorer();
    for kind in ["MimicWorkload", "MimicLoadTest", "MimicConfig"] {
        let path = format!("/ui/workloads/shop/front/resources/{kind}/some.name-1");
        assert_eq!(fetch(&app, &path).await.status, 200, "{path}");
    }
    for path in [
        "/ui/workloads/shop/front/resources/Secret/x",
        "/ui/workloads/shop/front/resources/mimicworkload/x",
        "/ui/workloads/shop/front/resources/MimicWorkload/Bad_Name",
        "/ui/workloads/shop/front/resources/MimicWorkload/-x",
        "/ui/workloads/Bad_Ns/front/resources/MimicWorkload/x",
    ] {
        assert_eq!(fetch(&app, path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn exp_04_head_405_and_403_hold_on_the_new_routes() {
    let (app, _) = explorer();
    let local = Some("localhost:8080");
    for path in NEW_ROUTES {
        let get = fetch(&app, path).await;
        let head = send(&app, Method::HEAD, path, "127.0.0.1", local).await;
        assert_eq!(head.status, get.status, "{path}");
        assert_eq!(head.headers["content-type"], get.headers["content-type"]);
        assert!(head.body.is_empty(), "{path}");
        for method in [Method::POST, Method::DELETE] {
            let other = send(&app, method, path, "127.0.0.1", local).await;
            assert_eq!(other.status, 405, "{path}");
            assert_eq!(other.headers["allow"], "GET, HEAD", "{path}");
        }
        for (peer, host) in [
            ("10.1.2.3", local),
            ("127.0.0.1", Some("evil.example")),
            ("127.0.0.1", None),
        ] {
            let denied = send(&app, Method::GET, path, peer, host).await;
            assert_eq!(denied.status, 403, "{path} {peer} {host:?}");
        }
        for header in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
            "x-frame-options",
            "cross-origin-opener-policy",
            "cross-origin-resource-policy",
            "cache-control",
        ] {
            assert!(get.headers.contains_key(header), "{path}: {header}");
        }
        assert!(!get.headers.contains_key("access-control-allow-origin"));
    }
}

#[tokio::test]
async fn exp_05_hostile_values_stay_escaped_json_strings() {
    let (app, _) = explorer();
    let answer = fetch(&app, "/v1/workloads/shop/front/resources").await;
    assert_eq!(answer.headers["content-type"], "application/json");
    assert_eq!(answer.headers["x-content-type-options"], "nosniff");
    let body = json_of(&answer);
    let front = &body["resources"][2];
    assert_eq!(front["metadata"]["labels"]["team"], HOSTILE);
    assert_eq!(front["metadata"]["annotations"]["note"], HOSTILE);
    assert_eq!(front["conditions"][0]["message"], HOSTILE);
}

#[tokio::test]
async fn exp_05_a_load_test_shows_the_secret_name_and_field_names_only() {
    let (app, _) = explorer();
    let body = json_of(&fetch(&app, "/v1/workloads/shop/front/resources").await);
    let test = &body["resources"][0];
    assert_eq!(test["kind"], "MimicLoadTest");
    assert_eq!(test["spec"]["credentials"]["secretName"], "payment-load");
    assert_eq!(
        test["spec"]["credentials"]["fields"],
        json!([{"field": "header/authorization"}])
    );
    let text = test.to_string();
    assert!(!text.contains("\"key\""), "a Secret key name is rendered");
    assert!(!text.contains("token"));
}

#[test]
fn exp_05_mimic_js_encodes_the_names_of_its_links() {
    let js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/mimic.js")).unwrap();
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
    ] {
        assert!(!js.contains(forbidden), "{forbidden}");
    }
    assert!(js.contains("/resources/"), "no resource link");
    assert!(js.contains("/v1/resources"), "the overview is not read");
    assert!(
        js.contains("enc(") && js.contains("encodeURIComponent"),
        "links are not encoded"
    );
}

#[tokio::test]
async fn exp_06_the_new_routes_count_by_pattern_without_names() {
    let (app, metrics) = explorer();
    for path in NEW_ROUTES {
        assert_eq!(fetch(&app, path).await.status, 200, "{path}");
    }
    fetch(&app, "/v1/workloads/Bad_Ns/front/resources").await;
    fetch(&app, "/ui/workloads/shop/front/resources/Secret/x").await;
    for (route, code) in [
        ("/v1/resources", 200),
        ("/v1/workloads/{namespace}/{deployment}/resources", 200),
        ("/v1/workloads/{namespace}/{deployment}/resources", 400),
        ("/ui/map", 200),
        ("/ui/endpoints", 200),
        ("/ui/loadtests", 200),
        (
            "/ui/workloads/{namespace}/{deployment}/resources/{kind}/{name}",
            200,
        ),
        (
            "/ui/workloads/{namespace}/{deployment}/resources/{kind}/{name}",
            400,
        ),
    ] {
        assert_eq!(requests(&metrics, route, code), Some(1), "{route} {code}");
    }
    let text = metrics.encode();
    for name in ["shop", "front", "burst", "MimicLoadTest"] {
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("mimic_ui_") && l.contains(name)),
            "{name} is in a series"
        );
    }
}
