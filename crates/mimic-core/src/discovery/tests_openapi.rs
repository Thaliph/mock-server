//! UI-01, UI-02 and UI-05 (servers, latency, last seen): the OpenAPI mapping of spec §7.6.

use std::sync::OnceLock;

use jsonschema::{Retrieve, Uri, Validator};
use proptest::prelude::*;
use serde_json::{Value, json};

use super::histogram::{Bound, Histogram};
use super::observed::Observation;
use super::openapi::{OPENAPI_VERSION, document, endpoint_list, index, parse_id};
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, merge};

pub(super) const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    max_authorities: 64,
    sample_budget: 16 << 20,
};
const GUID_A: &str = "123e4567-e89b-12d3-a456-426614174000";
const GUID_B: &str = "223e4567-e89b-12d3-a456-426614174001";

const SCHEMAS: [(&str, &str); 4] = [
    (
        "https://spec.openapis.org/oas/3.1/schema-base/2022-10-07",
        include_str!("../../tests/fixtures/oas-3.1/schema-base.json"),
    ),
    (
        "https://spec.openapis.org/oas/3.1/schema/2022-10-07",
        include_str!("../../tests/fixtures/oas-3.1/schema.json"),
    ),
    (
        "https://spec.openapis.org/oas/3.1/dialect/base",
        include_str!("../../tests/fixtures/oas-3.1/dialect-base.json"),
    ),
    (
        "https://spec.openapis.org/oas/3.1/meta/base",
        include_str!("../../tests/fixtures/oas-3.1/meta-base.json"),
    ),
];

/// Serves the vendored schemas and nothing else: no network.
struct Vendored;

impl Retrieve for Vendored {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let (_, text) = SCHEMAS
            .iter()
            .find(|(id, _)| *id == uri.as_str())
            .ok_or_else(|| format!("not vendored: {uri}"))?;
        Ok(serde_json::from_str(text)?)
    }
}

fn validator() -> &'static Validator {
    static V: OnceLock<Validator> = OnceLock::new();
    V.get_or_init(|| {
        let base: Value = serde_json::from_str(SCHEMAS[0].1).expect("fixture");
        jsonschema::options()
            .with_retriever(Vendored)
            .build(&base)
            .expect("the OAS 3.1 schema-base compiles")
    })
}

/// The validation errors of a document, empty when it is valid.
pub(super) fn errors(doc: &Value) -> Vec<String> {
    validator()
        .iter_errors(doc)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect()
}

#[derive(Debug, Clone)]
struct Ex {
    port: u16,
    method: String,
    path: String,
    grpc: bool,
    status: u16,
    ms: u64,
    host: Option<String>,
}

fn ex(port: u16, method: &str, path: &str) -> Ex {
    Ex {
        port,
        method: method.into(),
        path: path.into(),
        grpc: false,
        status: 200,
        ms: 10,
        host: None,
    }
}

fn record(report: &mut Report, e: &Ex) {
    report.record(
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: e.port,
            method: &e.method,
            path: &e.path,
            status: e.status,
            head_ms: e.ms,
            host: e.host.as_deref(),
            grpc: e.grpc,
            observation: Observation::default(),
        },
    );
}

fn report_of(exchanges: &[Ex]) -> Report {
    let mut r = Report::new(None);
    exchanges.iter().for_each(|e| record(&mut r, e));
    r
}

fn doc(report: &Report, id: &str) -> Value {
    document(report, "ns", "dep", 3, id).expect("the document exists")
}

fn paths(doc: &Value) -> Vec<&str> {
    doc["paths"]
        .as_object()
        .expect("paths")
        .keys()
        .map(String::as_str)
        .collect()
}

pub(super) fn assert_valid(doc: &Value) {
    let problems = errors(doc);
    assert!(problems.is_empty(), "{problems:?}");
}

// ---------------------------------------------------------------------------------------
// UI-02

#[test]
fn ui_02_template_and_path_parameter() {
    let r = report_of(&[ex(8080, "GET", "/users/42"), ex(8080, "PUT", "/users/7")]);
    let d = doc(&r, "inbound-8080");
    assert_eq!(paths(&d), ["/users/{users_id}"]);
    let item = &d["paths"]["/users/{users_id}"];
    for method in ["get", "put"] {
        assert_eq!(
            item[method]["parameters"],
            json!([{"name": "users_id", "in": "path", "required": true, "schema": {"type": "string"}}]),
            "{method}"
        );
    }
    assert_valid(&d);
}

#[test]
fn ui_02_parameter_format_from_the_sample_values() {
    let uuids = report_of(&[
        ex(80, "GET", &format!("/users/{GUID_A}")),
        ex(80, "GET", &format!("/users/{GUID_B}")),
    ]);
    let schema = |r: &Report| {
        doc(r, "inbound-80")["paths"]["/users/{users_id}"]["get"]["parameters"][0]["schema"].clone()
    };
    assert_eq!(schema(&uuids), json!({"type": "string", "format": "uuid"}));
    let mixed = report_of(&[
        ex(80, "GET", &format!("/users/{GUID_A}")),
        ex(80, "GET", "/users/42"),
    ]);
    assert_eq!(schema(&mixed), json!({"type": "string"}));
}

#[test]
fn ui_02_root_and_trailing_slash_are_their_own_keys() {
    let r = report_of(&[
        ex(80, "GET", "/"),
        ex(80, "GET", "/users"),
        ex(80, "GET", "/users/"),
    ]);
    let d = doc(&r, "inbound-80");
    assert_eq!(paths(&d), ["/", "/users", "/users/"]);
    assert_valid(&d);
}

#[test]
fn ui_02_braces_of_a_literal_are_escaped() {
    let r = report_of(&[ex(80, "GET", "/a{b}"), ex(80, "GET", "/c/42")]);
    let d = doc(&r, "inbound-80");
    assert_eq!(paths(&d), ["/a%7Bb%7D", "/c/{c_id}"]);
    // A literal is not a path parameter.
    assert!(d["paths"]["/a%7Bb%7D"]["get"].get("parameters").is_none());
    assert_valid(&d);
}

#[test]
fn ui_02_standard_methods_and_x_mimic_methods() {
    let standard = [
        "GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE",
    ];
    let mut all: Vec<Ex> = standard.iter().map(|m| ex(80, m, "/x")).collect();
    all.push(ex(80, "PROPFIND", "/x"));
    all.push(ex(80, "get", "/x"));
    let d = doc(&report_of(&all), "inbound-80");
    let item = d["paths"]["/x"].as_object().expect("path item");
    let keys: Vec<&str> = item.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "delete",
            "get",
            "head",
            "options",
            "patch",
            "post",
            "put",
            "trace",
            "x-mimic-methods"
        ]
    );
    let extra = item["x-mimic-methods"].as_object().expect("extra methods");
    assert_eq!(
        extra.keys().map(String::as_str).collect::<Vec<_>>(),
        ["PROPFIND", "get"]
    );
    assert_eq!(extra["PROPFIND"]["x-mimic-exchanges"], 1);
    assert_valid(&d);
}

#[test]
fn ui_02_one_document_per_port() {
    let r = report_of(&[ex(9090, "GET", "/a"), ex(8080, "GET", "/a")]);
    for (id, port) in [("inbound-8080", 8080), ("inbound-9090", 9090)] {
        let d = doc(&r, id);
        assert_eq!(paths(&d), ["/a"], "{id}");
        assert_eq!(d["info"]["title"], format!("ns/dep inbound :{port}"));
    }
    assert!(document(&r, "ns", "dep", 3, "inbound-80").is_none());
    assert!(document(&r, "ns", "dep", 3, "foo").is_none());
}

#[test]
fn ui_02_grpc_path_is_a_key_with_tag() {
    let mut e = ex(50051, "POST", "/pkg.Svc/Get");
    e.grpc = true;
    let d = doc(&report_of(&[e]), "inbound-50051");
    assert_eq!(paths(&d), ["/pkg.Svc/Get"]);
    let op = &d["paths"]["/pkg.Svc/Get"]["post"];
    assert_eq!(op["tags"], json!(["pkg.Svc"]));
    assert_eq!(op["x-mimic-grpc"], true);
    assert!(op.get("parameters").is_none());
    assert_valid(&d);
}

#[test]
fn ui_02_id_grammar() {
    for (id, port) in [
        ("inbound-1", 1),
        ("inbound-8080", 8080),
        ("inbound-65535", 65535),
    ] {
        assert_eq!(parse_id(id), Some(port), "{id}");
    }
    for id in [
        "inbound-0",
        "inbound-65536",
        "inbound-08080",
        "inbound-",
        "inbound-+1",
        "inbound-1 ",
        "foo",
        "outbound-80",
        "inbound-123456",
    ] {
        assert_eq!(parse_id(id), None, "{id}");
    }
}

#[test]
fn ui_02_index_and_endpoint_list() {
    let mut not_found = ex(9090, "GET", "/b");
    not_found.status = 404;
    let r = report_of(&[
        ex(9090, "GET", "/b"),
        ex(8080, "PUT", "/users/7"),
        ex(8080, "GET", "/users/42"),
        not_found,
        ex(8080, "GET", "/"),
    ]);
    assert_eq!(
        index(&r),
        json!({"documents": [
            {"id": "inbound-8080", "direction": "inbound", "port": 8080, "endpoints": 3},
            {"id": "inbound-9090", "direction": "inbound", "port": 9090, "endpoints": 1},
        ]})
    );
    let list = endpoint_list(&r);
    let rows: Vec<(&str, &str, &str, u64)> = list["endpoints"]
        .as_array()
        .expect("endpoints")
        .iter()
        .map(|e| {
            (
                e["document"].as_str().expect("document"),
                e["method"].as_str().expect("method"),
                e["template"].as_str().expect("template"),
                e["exchanges"].as_u64().expect("exchanges"),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("inbound-8080", "GET", "/", 1),
            ("inbound-8080", "GET", "/users/{users_id}", 1),
            ("inbound-8080", "PUT", "/users/{users_id}", 1),
            ("inbound-9090", "GET", "/b", 2),
        ]
    );
    let last = &list["endpoints"][3];
    assert_eq!(last["direction"], "inbound");
    assert_eq!(last["port"], 9090);
    assert_eq!(last["grpc"], false);
    assert_eq!(last["statuses"], json!({"200": 1, "404": 1}));
    assert_eq!(last["latency_ms"], json!({"p50": 12, "p90": 12, "p99": 12}));
    assert_eq!(last["hosts"], json!([]));
    assert!(last.get("last_seen").is_none());
}

// ---------------------------------------------------------------------------------------
// UI-05

fn servers(hosts: &[&str]) -> Option<Value> {
    let mut all: Vec<Ex> = hosts
        .iter()
        .map(|h| Ex {
            host: Some((*h).into()),
            ..ex(80, "GET", "/x")
        })
        .collect();
    // An exchange without a host, so that the document exists.
    all.push(ex(80, "GET", "/x"));
    let d = doc(&report_of(&all), "inbound-80");
    assert_valid(&d);
    d.get("servers").cloned()
}

#[test]
fn ui_05_servers_hold_the_valid_hosts_sorted() {
    assert_eq!(
        servers(&["users.ns.svc:8080", "(invalid)", "b.example", "a.example"]),
        Some(json!([
            {"url": "http://a.example"},
            {"url": "http://b.example"},
            {"url": "http://users.ns.svc:8080"},
        ]))
    );
    assert_eq!(servers(&["(invalid)"]), None);
    assert_eq!(servers(&[]), None);
}

#[test]
fn ui_05_servers_skip_a_host_with_the_marker() {
    // A host the sidecar redacted is the marker; one that merely contains it is not a host
    // either.
    assert_eq!(
        servers(&["AKIAIOSFODNN7EXAMPLE", "<redacted>", "a<redacted>b"]),
        None
    );
}

#[test]
fn ui_05_servers_hold_at_most_16_hosts() {
    // 20 distinct valid hosts in the scope, spread over 3 endpoints.
    let all: Vec<Ex> = (0..20)
        .map(|i| Ex {
            host: Some(format!("h{i:02}.example")),
            ..ex(80, "GET", &format!("/e{}", i % 3))
        })
        .collect();
    let d = doc(&report_of(&all), "inbound-80");
    let urls: Vec<&str> = d["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .map(|s| s["url"].as_str().expect("url"))
        .collect();
    assert_eq!(urls.len(), 16);
    assert!(urls.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(urls[0], "http://h00.example");
}

#[test]
fn ui_05_histogram_quantiles() {
    let mut h = Histogram::default();
    assert_eq!(h.quantile(50), None);
    (0..100).for_each(|_| h.observe(10));
    // 10 ms is in the bucket closing at 2^(14/4) = 11.3 ms, rounded up.
    for p in [50, 90, 99, 100] {
        assert_eq!(h.quantile(p), Some(Bound::Ms(12)), "p{p}");
    }
    let mut mixed = Histogram::default();
    (0..90).for_each(|_| mixed.observe(1));
    (0..9).for_each(|_| mixed.observe(100));
    mixed.observe(1 << 20);
    assert_eq!(mixed.quantile(50), Some(Bound::Ms(1)));
    assert_eq!(mixed.quantile(90), Some(Bound::Ms(1)));
    // 100 ms is in the bucket closing at 2^(27/4) = 107.6 ms.
    assert_eq!(mixed.quantile(99), Some(Bound::Ms(108)));
    assert_eq!(mixed.quantile(100), Some(Bound::Overflow));
}

#[test]
fn ui_05_quantile_of_an_out_of_range_bucket_does_not_panic() {
    // `from_json` alone does not run the §7.3 check, so a bucket past the overflow bucket can
    // reach `quantile`: it counts as overflow.
    let h: Histogram =
        serde_json::from_str(r#"{"buckets":{"10":1,"70":1,"255":1}}"#).expect("parses");
    assert_eq!(h.quantile(30), Some(Bound::Ms(6)));
    for p in [50, 99, 100] {
        assert_eq!(h.quantile(p), Some(Bound::Overflow), "p{p}");
    }
}

#[test]
fn ui_01_refs_outside_the_allowed_places_are_refused() {
    let allowed = json!({"schema": {"properties": {"$ref": {"type": "string"}}},
        "examples": {"sample-1": {"value": {"$ref": "http://h/x", "a": {"$ref": 1}}}}});
    assert!(refs_ok(&allowed));
    for bad in [
        json!({"paths": {"/x": {"get": {"$ref": "#/x"}}}}),
        json!({"content": {"$ref": {}}}),
        json!({"schema": {"$ref": "http://h/x"}}),
        json!([{"$ref": "x"}]),
        // Neither a schema's `properties` nor an example `value`: no free subtree.
        json!({"properties": {"$ref": {}}}),
        json!({"example": {"$ref": 1}}),
        json!({"schema": {"properties": {"a": {"$ref": "x"}}}}),
        json!({"examples": {"s": {"$ref": "x"}}}),
        json!({"examples": {"s": {"value": 1, "other": {"$ref": "x"}}}}),
    ] {
        assert!(!refs_ok(&bad), "{bad}");
    }
}

#[test]
fn ui_05_latency_in_the_operation() {
    let all: Vec<Ex> = (0..100).map(|_| ex(80, "GET", "/x")).collect();
    let d = doc(&report_of(&all), "inbound-80");
    let op = &d["paths"]["/x"]["get"];
    assert_eq!(
        op["x-mimic-latency-ms"],
        json!({"p50": 12, "p90": 12, "p99": 12})
    );
    assert_eq!(op["x-mimic-exchanges"], 100);
    let slow = Ex {
        ms: 1 << 20,
        ..ex(80, "GET", "/slow")
    };
    let d = doc(&report_of(&[slow]), "inbound-80");
    assert_eq!(
        d["paths"]["/slow"]["get"]["x-mimic-latency-ms"],
        json!({"p50": null, "p90": null, "p99": null})
    );
    assert_valid(&d);
}

#[test]
fn ui_05_last_seen_is_the_stamped_day() {
    let mut r = report_of(&[ex(80, "GET", "/x")]);
    let seen = |r: &Report| doc(r, "inbound-80")["paths"]["/x"]["get"]["x-mimic-last-seen"].clone();
    assert_eq!(seen(&r), Value::Null);
    assert!(endpoint_list(&r)["endpoints"][0].get("last_seen").is_none());
    // Day 20000 since 1970-01-01 is 2024-10-04.
    r.stamp(20_000);
    assert_eq!(seen(&r), "2024-10-04");
    assert_eq!(endpoint_list(&r)["endpoints"][0]["last_seen"], "2024-10-04");
    r.stamp(0);
    assert_eq!(seen(&r), "1970-01-01");
    r.stamp(11_016);
    assert_eq!(seen(&r), "2000-02-29");
}

// ---------------------------------------------------------------------------------------
// UI-01

#[test]
fn ui_01_top_level() {
    let r = report_of(&[ex(80, "GET", "/x")]);
    let d = doc(&r, "inbound-80");
    assert_eq!(d["openapi"], OPENAPI_VERSION);
    assert_eq!(OPENAPI_VERSION, "3.1.1");
    assert_eq!(d["info"]["title"], "ns/dep inbound :80");
    assert_eq!(d["info"]["version"], "3");
    let other = document(&r, "ns", "dep", 4, "inbound-80").expect("document");
    assert_eq!(other["info"]["version"], "4");
    assert_eq!(other["info"]["description"], d["info"]["description"]);
    assert!(
        d["info"]["description"]
            .as_str()
            .expect("description")
            .contains("observed traffic")
    );
}

/// Where a value sits in a document, for the `$ref` rule.
#[derive(Clone, Copy, PartialEq)]
enum At {
    /// Anything that is none of the below: no `$ref` member at all.
    Plain,
    /// A schema: `properties`, `items` and `additionalProperties` hold schemas.
    Schema,
    /// The `properties` map of a schema: its keys are free, its values are schemas.
    Properties,
    /// An `examples` map of example objects.
    Examples,
    /// An example object: only its `value` is free.
    Example,
    /// An example `value`.
    Value,
}

/// No `$ref` member other than a key of a schema's `properties` map and inside the `value` of
/// an example: exactly those places, not whole subtrees around them.
pub(super) fn refs_ok(v: &Value) -> bool {
    refs_at(v, At::Plain)
}

fn refs_at(v: &Value, at: At) -> bool {
    match (v, at) {
        (_, At::Value) => true,
        (Value::Object(map), _) => map.iter().all(|(k, child)| {
            let below = match (at, k.as_str()) {
                (At::Properties, _) => At::Schema,
                (At::Examples, _) => At::Example,
                (At::Example, "value") => At::Value,
                (At::Plain, "schema") => At::Schema,
                (At::Plain, "examples") => At::Examples,
                (At::Schema, "properties") => At::Properties,
                (At::Schema, "items" | "additionalProperties") => At::Schema,
                _ => At::Plain,
            };
            (k != "$ref" || at == At::Properties) && refs_at(child, below)
        }),
        (Value::Array(items), _) => items.iter().all(|i| refs_at(i, At::Plain)),
        _ => true,
    }
}

fn arb_ex() -> impl Strategy<Value = Ex> {
    let paths = prop::sample::select(vec![
        "/",
        "/users",
        "/users/",
        "/users/42",
        "/users/7",
        "/a{b}",
        "/a/{x}",
        "/orders/1/items/2",
        "/orders/9/items/x",
        "/pkg.Svc/Get",
    ]);
    let guid = prop::sample::select(vec![GUID_A, GUID_B]).prop_map(|g| format!("/users/{g}"));
    (
        prop::sample::select(vec![8080u16, 9090]),
        prop::sample::select(vec!["GET", "PUT", "POST", "PROPFIND"]),
        prop_oneof![paths.prop_map(String::from), guid],
        prop::sample::select(vec![200u16, 201, 500, 503]),
        prop::sample::select(vec![1u64, 10, 100, 5000, 1 << 20]),
        prop::sample::select(vec![None, Some("svc"), Some("svc:8080"), Some("(invalid)")]),
    )
        .prop_map(|(port, method, path, status, ms, host)| Ex {
            port,
            method: method.into(),
            grpc: path.starts_with("/pkg."),
            path,
            status,
            ms,
            host: host.map(String::from),
        })
}

fn rendered(report: &Report) -> (String, String, Vec<String>) {
    let ids: Vec<String> = index(report)["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|d| d["id"].as_str().expect("id").to_string())
        .collect();
    let docs = ids.iter().map(|id| doc(report, id).to_string()).collect();
    (
        index(report).to_string(),
        endpoint_list(report).to_string(),
        docs,
    )
}

fn merged(exchanges: &[Ex], chunk: usize, reverse: bool) -> Report {
    let mut parts: Vec<Report> = exchanges.chunks(chunk).map(report_of).collect();
    if reverse {
        parts.reverse();
    }
    parts
        .into_iter()
        .reduce(|a, b| merge(a, b, &LIMITS))
        .unwrap_or_else(|| Report::new(None))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn ui_01_deterministic_valid_and_independent_of_the_merge_order(
        exchanges in prop::collection::vec(arb_ex(), 0..12),
        chunk in 1usize..5,
    ) {
        let whole = merged(&exchanges, exchanges.len().max(1), false);
        let want = rendered(&whole);
        prop_assert_eq!(&want, &rendered(&whole));
        prop_assert_eq!(&want, &rendered(&merged(&exchanges, chunk, false)));
        prop_assert_eq!(&want, &rendered(&merged(&exchanges, chunk, true)));
        let mut ports: Vec<u16> = exchanges.iter().map(|e| e.port).collect();
        ports.sort_unstable();
        ports.dedup();
        prop_assert_eq!(want.2.len(), ports.len());
        for text in &want.2 {
            let d: Value = serde_json::from_str(text).unwrap();
            let problems = errors(&d);
            prop_assert!(problems.is_empty(), "{:?}\n{}", problems, text);
            prop_assert_eq!(&d["openapi"], "3.1.1");
            prop_assert_eq!(&d["info"]["version"], "3");
            let title = d["info"]["title"].as_str().unwrap();
            prop_assert!(title.starts_with("ns/dep inbound :"), "{}", title);
            prop_assert!(refs_ok(&d), "{}", text);
        }
    }
}

#[test]
fn ui_01_the_validator_rejects_an_invalid_document() {
    // The schema fixture is really applied: a wrong version and a stray member are refused.
    let info = json!({"title": "t", "version": "1"});
    assert!(!errors(&json!({"openapi": "3.0.0", "info": info, "paths": {}})).is_empty());
    assert!(
        !errors(&json!({"openapi": "3.1.1", "info": info, "paths": {}, "stray": 1})).is_empty()
    );
    assert!(errors(&json!({"openapi": "3.1.1", "info": info, "paths": {}})).is_empty());
}
