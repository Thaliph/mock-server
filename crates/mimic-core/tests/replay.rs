//! Critères RPL-* de la spec (§5.7) : liste des requêtes enregistrées et rejeu.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::model::{Body, Direction, Exchange};
use mimic_core::replay::{k6_script, recent_requests};
use serde_json::{Value, json};

fn inbound(method: &str, path: &str, at: u64, id: &str) -> Exchange {
    exchange(method, path).at(at, id)
}

#[test]
fn rpl_01_most_recent_first_inbound_only_without_duplicates() {
    let mut out = inbound("GET", "/x", 5, "o");
    out.direction = Direction::Outbound;
    let list = vec![
        inbound("GET", "/a", 1, "pod-a-1"),
        inbound("GET", "/b", 3, "pod-b-1"),
        out,
        inbound("GET", "/c", 2, "pod-a-2"),
        inbound("GET", "/b", 3, "pod-b-1"),
    ];
    let r = recent_requests(list, Direction::Inbound, 10);
    let ids: Vec<_> = r.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["pod-b-1", "pod-a-2", "pod-a-1"]);
    let r = recent_requests(r, Direction::Inbound, 2);
    assert_eq!(r.len(), 2);
}

/// Extrait le tableau de requêtes embarqué dans le script.
fn embedded(script: &str) -> Vec<Value> {
    let start = script.find("const REQUESTS = ").unwrap() + "const REQUESTS = ".len();
    let end = script[start..].find(";\n").unwrap() + start;
    serde_json::from_str(&script[start..end]).unwrap()
}

#[test]
fn rpl_02_k6_script_replays_requests_without_redacted_headers() {
    let a = inbound("POST", "/orders?coupon=a b&x=%", 2, "2")
        .req_json(json!({"sku": 4}))
        .with_req_header("authorization", "<redacted:sha256:0123abcd>")
        .with_req_header("host", "checkout:8080")
        .with_req_header("x-tenant", "t1");
    let b = inbound("GET", "/orders/4", 1, "1");
    let mut binary = inbound("PUT", "/blob", 3, "3");
    binary.req_body = Body::captured(vec![0xff, 0x00, 0x10]);
    let script = k6_script(&[a, b, binary]);

    assert!(script.contains("import http from 'k6/http';"));
    assert!(script.contains("__ENV.TARGET"));
    assert!(script.contains("__ENV.AUTHORIZATION"));
    assert!(script.contains("export default function"));
    let reqs = embedded(&script);
    // Ordre chronologique d'enregistrement.
    let paths: Vec<_> = reqs.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/orders/4", "/orders", "/blob"]);
    let post = &reqs[1];
    assert_eq!(post["method"], "POST");
    assert_eq!(post["query"], json!([["coupon", "a b"], ["x", "%"]]));
    assert_eq!(post["body"], r#"{"sku":4}"#);
    assert_eq!(post["headers"]["x-tenant"], "t1");
    assert_eq!(post["headers"]["content-type"], "application/json");
    assert!(post["headers"].get("authorization").is_none());
    assert!(post["headers"].get("host").is_none());
    assert_eq!(reqs[2]["body_b64"], "/wAQ");
    assert!(reqs[0]["body"].is_null());
}

#[test]
fn k6_script_of_nothing_is_still_valid() {
    let script = k6_script(&[]);
    assert!(embedded(&script).is_empty());
}
