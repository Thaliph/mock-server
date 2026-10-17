//! Critères d'acceptation INF-* de la spec (§11.2) : inférence OpenAPI.

mod common;

use common::{ExchangeExt, exchange, openapi_violations, sequence};
use mimic_core::infer::{InferOptions, infer};
use mimic_core::model::{Direction, Exchange};
use proptest::prelude::*;
use serde_json::{Value, json};

fn doc(exchanges: Vec<Exchange>) -> Value {
    infer(&sequence(exchanges), &InferOptions::new("svc", "0.0.0"))
}

fn path_keys(doc: &Value) -> Vec<String> {
    doc["paths"].as_object().unwrap().keys().cloned().collect()
}

fn path_param<'a>(doc: &'a Value, path: &str, method: &str, name: &str) -> &'a Value {
    doc["paths"][path][method]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["in"] == "path" && p["name"] == name)
        .unwrap_or_else(|| panic!("paramètre {name} absent de {method} {path}"))
}

fn response_schema<'a>(doc: &'a Value, path: &str, method: &str, status: &str) -> &'a Value {
    &doc["paths"][path][method]["responses"][status]["content"]["application/json"]["schema"]
}

#[test]
fn inf_01_numeric_segment_becomes_integer_param() {
    let exchanges = (1..=10)
        .map(|n| exchange("GET", &format!("/users/{n}")))
        .collect();
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/users/{userId}"]);
    let param = path_param(&doc, "/users/{userId}", "get", "userId");
    assert_eq!(param["required"], true);
    assert_eq!(param["schema"]["type"], "integer");
}

#[test]
fn inf_02_frequent_literal_is_kept_beside_param() {
    let mut exchanges: Vec<_> = (1..=50)
        .map(|n| exchange("GET", &format!("/users/{n}")))
        .collect();
    exchanges.extend((0..20).map(|_| exchange("GET", "/users/me")));
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/users/me", "/users/{userId}"]);
}

#[test]
fn inf_03_below_cardinality_threshold_stays_literal() {
    let exchanges = (1..=3)
        .map(|n| exchange("GET", &format!("/items/{n}")))
        .collect();
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/items/1", "/items/2", "/items/3"]);
}

#[test]
fn inf_04_required_reflects_presence_ratio() {
    let exchanges = (0..10)
        .map(|i| {
            let body = if i % 2 == 0 {
                json!({"email": "a@b.io", "phone": "+33100000000"})
            } else {
                json!({"email": "a@b.io"})
            };
            exchange("GET", "/me").resp_json(body)
        })
        .collect();
    let doc = doc(exchanges);
    let schema = response_schema(&doc, "/me", "get", "200");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(["email"]));
    assert!(schema["properties"]["phone"].is_object());
}

#[test]
fn inf_05_nullable_field_uses_type_array() {
    let exchanges = vec![
        exchange("GET", "/x").resp_json(json!({"name": "x"})),
        exchange("GET", "/x").resp_json(json!({"name": null})),
    ];
    let doc = doc(exchanges);
    let name = &response_schema(&doc, "/x", "get", "200")["properties"]["name"];
    assert_eq!(name["type"], json!(["string", "null"]));
}

#[test]
fn inf_06_low_cardinality_string_becomes_enum() {
    let states = ["pending", "paid", "shipped"];
    let exchanges = (0..100)
        .map(|i| exchange("GET", "/order").resp_json(json!({"status": states[i % 3]})))
        .collect();
    let doc = doc(exchanges);
    let status = &response_schema(&doc, "/order", "get", "200")["properties"]["status"];
    assert_eq!(status["enum"], json!(["paid", "pending", "shipped"]));
}

#[test]
fn inf_06b_enum_needs_enough_observations() {
    let exchanges = (0..10)
        .map(|_| exchange("GET", "/order").resp_json(json!({"status": "paid"})))
        .collect();
    let doc = doc(exchanges);
    let status = &response_schema(&doc, "/order", "get", "200")["properties"]["status"];
    assert!(status.get("enum").is_none(), "{status}");
}

#[test]
fn string_formats_are_detected() {
    let exchanges = vec![exchange("GET", "/f").resp_json(json!({
        "at": "2026-10-17T08:33:09Z",
        "day": "2026-10-17",
        "id": "0b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b10",
        "mail": "jane@example.com",
        "site": "https://example.com/a",
        "ip": "10.0.0.1",
        "plain": "hello",
    }))];
    let doc = doc(exchanges);
    let props = &response_schema(&doc, "/f", "get", "200")["properties"];
    assert_eq!(props["at"]["format"], "date-time");
    assert_eq!(props["day"]["format"], "date");
    assert_eq!(props["id"]["format"], "uuid");
    assert_eq!(props["mail"]["format"], "email");
    assert_eq!(props["site"]["format"], "uri");
    assert_eq!(props["ip"]["format"], "ipv4");
    assert!(props["plain"].get("format").is_none());
}

#[test]
fn mixed_types_use_one_of_and_integer_widens_to_number() {
    let exchanges = vec![
        exchange("GET", "/m").resp_json(json!({"v": 1, "n": 1})),
        exchange("GET", "/m").resp_json(json!({"v": "one", "n": 1.5})),
    ];
    let doc = doc(exchanges);
    let props = &response_schema(&doc, "/m", "get", "200")["properties"];
    assert_eq!(
        props["v"]["oneOf"],
        json!([{"type": "integer"}, {"type": "string"}])
    );
    assert_eq!(props["n"]["type"], "number");
}

#[test]
fn array_items_are_merged() {
    let exchanges = vec![exchange("GET", "/l").resp_json(json!([{"a": 1}, {"a": 2, "b": true}]))];
    let doc = doc(exchanges);
    let schema = response_schema(&doc, "/l", "get", "200");
    assert_eq!(schema["type"], "array");
    assert_eq!(schema["items"]["required"], json!(["a"]));
    assert_eq!(schema["items"]["properties"]["b"]["type"], "boolean");
}

#[test]
fn methods_status_codes_and_request_bodies_are_documented() {
    let exchanges = vec![
        exchange("POST", "/orders")
            .req_json(json!({"sku": "A1", "qty": 2}))
            .status(201)
            .resp_json(json!({"id": 1})),
        exchange("POST", "/orders")
            .req_json(json!({"sku": "B2", "qty": 1}))
            .status(422)
            .resp_json(json!({"error": "stock"})),
        exchange("GET", "/orders"),
    ];
    let doc = doc(exchanges);
    let post = &doc["paths"]["/orders"]["post"];
    let req = &post["requestBody"]["content"]["application/json"]["schema"];
    assert_eq!(req["required"], json!(["qty", "sku"]));
    assert!(post["responses"]["201"].is_object());
    assert!(post["responses"]["422"].is_object());
    assert!(doc["paths"]["/orders"]["get"]["responses"]["200"].is_object());
}

#[test]
fn query_params_are_typed_and_required_when_always_present() {
    let exchanges = (0..5)
        .map(|i| {
            let q = if i % 2 == 0 {
                format!("/s?page={i}&q=x&debug=true")
            } else {
                format!("/s?page={i}")
            };
            exchange("GET", &q)
        })
        .collect();
    let doc = doc(exchanges);
    let params = doc["paths"]["/s"]["get"]["parameters"].as_array().unwrap();
    let find = |n: &str| {
        params
            .iter()
            .find(|p| p["in"] == "query" && p["name"] == n)
            .unwrap()
    };
    assert_eq!(find("page")["required"], true);
    assert_eq!(find("page")["schema"]["type"], "integer");
    assert_eq!(find("q")["required"], false);
    assert_eq!(find("debug")["schema"]["type"], "boolean");
}

#[test]
fn custom_request_headers_become_params_but_standard_ones_do_not() {
    let exchanges = vec![
        exchange("GET", "/h")
            .with_req_header("X-Api-Version", "2")
            .with_req_header("User-Agent", "curl")
            .with_req_header("Authorization", "Bearer t"),
    ];
    let doc = doc(exchanges);
    let params = doc["paths"]["/h"]["get"]["parameters"].as_array().unwrap();
    let names: Vec<_> = params
        .iter()
        .filter(|p| p["in"] == "header")
        .map(|p| p["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("x-api-version")]);
}

#[test]
fn stats_extension_carries_counts_status_mix_and_latency() {
    let mut exchanges: Vec<_> = (1..=99)
        .map(|ms| exchange("GET", "/p").latency_ms(ms))
        .collect();
    exchanges.push(exchange("GET", "/p").status(500).latency_ms(1_000));
    let doc = doc(exchanges);
    let stats = &doc["paths"]["/p"]["get"]["x-mimic-stats"];
    assert_eq!(stats["count"], 100);
    assert_eq!(stats["status"]["200"], 0.99);
    assert_eq!(stats["status"]["500"], 0.01);
    assert_eq!(stats["latency_ms"]["p50"], 50.0);
    assert_eq!(stats["latency_ms"]["p99"], 99.0);
    assert_eq!(stats["latency_ms"]["max"], 1000.0);
}

#[test]
fn examples_are_bounded_and_taken_in_chronological_order() {
    let exchanges = (0..10)
        .map(|i| exchange("GET", "/e").resp_json(json!({"i": i})))
        .collect();
    let doc = doc(exchanges);
    let examples =
        &doc["paths"]["/e"]["get"]["responses"]["200"]["content"]["application/json"]["examples"];
    let examples = examples.as_object().unwrap();
    assert_eq!(examples.len(), InferOptions::new("t", "v").max_examples);
    assert_eq!(examples["sample-1"]["value"], json!({"i": 0}));
}

#[test]
fn param_names_are_derived_from_parent_and_kept_unique() {
    let exchanges = (1..=6)
        .map(|n| exchange("GET", &format!("/categories/{n}/{}", 100 + n)))
        .collect();
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/categories/{categoryId}/{param2}"]);
}

#[test]
fn uuid_segments_become_uuid_params_across_methods() {
    let ids = [
        "0b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b10",
        "1b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b11",
        "2b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b12",
        "3b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b13",
        "4b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b14",
    ];
    let mut exchanges: Vec<_> = ids
        .iter()
        .map(|id| exchange("GET", &format!("/carts/{id}")))
        .collect();
    exchanges.push(exchange("DELETE", &format!("/carts/{}", ids[0])));
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/carts/{cartId}"]);
    let p = path_param(&doc, "/carts/{cartId}", "delete", "cartId");
    assert_eq!(p["schema"], json!({"type": "string", "format": "uuid"}));
}

#[test]
fn words_are_not_absorbed_into_params() {
    let collections = ["users", "orders", "items", "products", "carts", "invoices"];
    let exchanges = collections
        .iter()
        .map(|c| exchange("GET", &format!("/api/{c}")))
        .collect();
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc).len(), collections.len());
}

#[test]
fn only_requested_direction_is_documented() {
    let mut out = exchange("GET", "/v1/charges");
    out.direction = Direction::Outbound;
    let exchanges = vec![exchange("GET", "/health"), out];
    let doc = doc(exchanges);
    assert_eq!(path_keys(&doc), vec!["/health"]);
}

#[test]
fn golden_petstore() {
    let fixture = include_str!("fixtures/petstore.jsonl");
    let exchanges: Vec<Exchange> = fixture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let doc = infer(&exchanges, &InferOptions::new("petstore", "1.0.0"));
    assert!(
        openapi_violations(&doc).is_empty(),
        "{:?}",
        openapi_violations(&doc)
    );
    insta::assert_json_snapshot!(doc);
}

fn arb_exchange() -> impl Strategy<Value = Exchange> {
    let segment = prop_oneof![
        Just("users".to_string()),
        Just("orders".to_string()),
        Just("me".to_string()),
        (0u32..500).prop_map(|n| n.to_string()),
        "[a-z]{1,6}-[0-9]{1,3}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i32>().prop_map(Value::from),
        "[a-z]{0,5}".prop_map(Value::from),
    ];
    let body = leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::from),
            prop::collection::btree_map("[a-c]", inner, 0..3)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    });
    (
        prop::sample::select(vec!["GET", "POST", "PUT", "DELETE", "PATCH"]),
        prop::collection::vec(segment, 0..4),
        prop::sample::select(vec![200u16, 201, 204, 404, 500]),
        prop::option::of(body),
        1u64..5_000,
    )
        .prop_map(|(method, segs, status, body, ms)| {
            let path = format!("/{}", segs.join("/"));
            let e = exchange(method, &path).status(status).latency_ms(ms);
            match body {
                Some(b) => e.resp_json(b),
                None => e,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn inf_07_any_corpus_yields_valid_openapi(exchanges in prop::collection::vec(arb_exchange(), 0..60)) {
        let doc = doc(exchanges);
        let errs = openapi_violations(&doc);
        prop_assert!(errs.is_empty(), "{:?}", errs);
    }

    #[test]
    fn inf_08_inference_is_order_independent(
        exchanges in prop::collection::vec(arb_exchange(), 1..60),
        seed in any::<u64>(),
    ) {
        let numbered = sequence(exchanges);
        let mut shuffled = numbered.clone();
        // Permutation déterministe dérivée de la graine.
        let mut state = seed | 1;
        for i in (1..shuffled.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            shuffled.swap(i, (state % (i as u64 + 1)) as usize);
        }
        let opts = InferOptions::new("svc", "0.0.0");
        prop_assert_eq!(infer(&numbered, &opts), infer(&shuffled, &opts));
    }
}
