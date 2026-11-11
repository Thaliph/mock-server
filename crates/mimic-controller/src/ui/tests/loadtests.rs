//! LDT-19: the load test routes of the read API (spec §7.5, §7.8).

use crate::store::load::{Admit, AdmitLimits, Ended};

use super::*;

const OLD: &str = "11111111-1111-4111-8111-111111111111";
const NEW: &str = "22222222-2222-4222-8222-222222222222";
const LIVE: &str = "33333333-3333-4333-8333-333333333333";

const SUMMARY: &str = r#"{"total":{"requests":30,"missed":1,"statuses":{"2xx":29,"5xx":1},"errors":{"timeout":2},"rps":15,"latencyMicros":{"p50":900,"p90":2000,"p99":5000,"max":9000}},"endpoints":[{"method":"GET","port":8080,"template":"/a","requests":30,"missed":1,"statuses":{"2xx":29,"5xx":1},"errors":{"timeout":2},"rps":15,"latencyMicros":{"p50":900,"p90":2000,"p99":5000,"max":9000}}]}"#;

async fn run(store: &Arc<Store>, ns: &str, uid: &str, name: &str, start: u64, end: Option<u64>) {
    let spec = format!(r#"{{"name":"{name}","deployment":"pay","concurrency":4}}"#);
    store
        .load_admit(&Admit {
            namespace: ns,
            deployment: "pay",
            uid,
            spec: &spec,
            plan: "{}",
            ticket: "v1.secret-ticket.signature",
            now_ms: start,
            deadline_ms: start + 600_000,
            reserve: 1 << 10,
            limits: AdmitLimits {
                max_runs: 1000,
                max_namespace_runs: 1000,
                budget: 1 << 40,
            },
        })
        .await
        .expect("admitted");
    if let Some(end_ms) = end {
        store
            .load_end(
                ns,
                "pay",
                uid,
                &Ended {
                    phase: "Succeeded",
                    reason: "Completed",
                    summary: SUMMARY,
                    end_ms,
                },
            )
            .await
            .expect("ended");
    }
}

#[tokio::test]
async fn ldt_19_list_is_newest_first_with_the_fields_of_a_run() {
    let store = store(&valkey_url());
    let ns = unique("ldt19a");
    run(&store, &ns, OLD, "first", 1_000_000, Some(1_100_000)).await;
    run(&store, &ns, NEW, "second", 2_000_000, Some(2_100_000)).await;
    run(&store, &ns, LIVE, "third", 3_000_000, None).await;
    let answer = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests")).await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    let runs = body["loadtests"].as_array().expect("a list");
    let uids: Vec<&str> = runs.iter().map(|r| r["uid"].as_str().unwrap()).collect();
    assert_eq!(uids, [LIVE, NEW, OLD], "{body}");
    let second = &runs[1];
    assert_eq!(second["name"], "second");
    assert_eq!(second["phase"], "Succeeded");
    assert_eq!(second["reason"], "Completed");
    assert_eq!(second["start"], "1970-01-01T00:33:20Z");
    assert_eq!(second["end"], "1970-01-01T00:35:00Z");
    assert_eq!(second["total"]["requests"], 30);
    assert_eq!(second["total"]["latencyMicros"]["p99"], 5000);
    assert_eq!(runs[0]["phase"], "Running");
    assert_eq!(runs[0]["end"], Value::Null);
    assert_eq!(runs[0]["total"], Value::Null);
    // Names and numbers only: the plan and the ticket never reach the page.
    assert!(!body.to_string().contains("secret-ticket"), "{body}");
}

#[tokio::test]
async fn ldt_19_list_of_a_deployment_without_runs_is_empty_and_names_are_checked() {
    let store = store(&valkey_url());
    let ns = unique("ldt19b");
    let answer = get(&store, &format!("/v1/workloads/{ns}/none/loadtests")).await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.json(), serde_json::json!({"loadtests": []}));
    let bad = get(&store, &format!("/v1/workloads/{ns}/Bad_Name/loadtests")).await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn ldt_19_run_answers_the_stored_summary_with_per_endpoint_results() {
    let store = store(&valkey_url());
    let ns = unique("ldt19c");
    run(&store, &ns, NEW, "second", 2_000_000, Some(2_100_000)).await;
    let answer = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests/{NEW}")).await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    assert_eq!(body["uid"], NEW);
    assert_eq!(body["name"], "second");
    assert_eq!(body["phase"], "Succeeded");
    let summary = &body["summary"];
    assert_eq!(summary["total"]["requests"], 30);
    assert_eq!(summary["endpoints"][0]["template"], "/a");
    assert_eq!(summary["endpoints"][0]["port"], 8080);
    assert_eq!(summary["endpoints"][0]["statuses"]["5xx"], 1);
    assert!(!body.to_string().contains("secret-ticket"), "{body}");
    // A run in progress has no summary yet.
    run(&store, &ns, LIVE, "third", 3_000_000, None).await;
    let live = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests/{LIVE}")).await;
    assert_eq!(live.status, 200);
    assert_eq!(live.json()["summary"], Value::Null);
}

#[tokio::test]
async fn ldt_19_unknown_uid_is_404_and_a_malformed_one_is_400() {
    let store = store(&valkey_url());
    let ns = unique("ldt19d");
    run(&store, &ns, NEW, "second", 2_000_000, Some(2_100_000)).await;
    let other = "44444444-4444-4444-8444-444444444444";
    let unknown = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests/{other}")).await;
    assert_eq!(unknown.status, 404);
    // Another deployment's run is not found under this one.
    let elsewhere = get(&store, &format!("/v1/workloads/{ns}/other/loadtests/{NEW}")).await;
    assert_eq!(elsewhere.status, 404);
    for bad in [
        "x",
        "2222222222224222822222222222222",
        "22222222-2222-4222-8222-22222222222G",
        "22222222-2222-4222-8222-2222222222222",
        "22222222-2222-4222-8222-22222222222A",
        "..",
    ] {
        let answer = get(&store, &format!("/v1/workloads/{ns}/pay/loadtests/{bad}")).await;
        assert_eq!(answer.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn ldt_19_loopback_rules_and_headers_apply() {
    let store = store(&valkey_url());
    let ns = unique("ldt19e");
    run(&store, &ns, NEW, "second", 2_000_000, Some(2_100_000)).await;
    for path in [
        format!("/v1/workloads/{ns}/pay/loadtests"),
        format!("/v1/workloads/{ns}/pay/loadtests/{NEW}"),
    ] {
        assert_eq!(
            call(&store, Method::GET, &path, "10.0.0.1").await.status,
            403
        );
        let rebinding = call_host(&store, Method::GET, &path, "127.0.0.1", Some("evil.test")).await;
        assert_eq!(rebinding.status, 403);
        assert_eq!(
            call(&store, Method::POST, &path, "127.0.0.1").await.status,
            405
        );
        let head = call(&store, Method::HEAD, &path, "127.0.0.1").await;
        assert_eq!((head.status, head.body.len()), (200, 0));
    }
}

#[test]
fn ldt_19_the_page_script_fills_a_load_tests_table_from_the_routes() {
    let script = include_str!("../../../ui/mimic.js");
    assert!(script.contains("Load tests"));
    assert!(script.contains("/loadtests"));
}
