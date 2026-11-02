//! UI-03, UI-04, UI-05 (security), UI-06 and the `$ref` rule of UI-01: parameters, bodies,
//! responses, security schemes and Markdown safety of the OpenAPI mapping (spec §7.6).

use proptest::prelude::*;
use serde_json::{Value, json};

use super::observed::Observation;
use super::openapi::{DESCRIPTION, document};
use super::redact::{MARKER, Redactor};
use super::report::{Direction, Exchange, Report, merge};
use super::schema::Schema;
use super::tests_openapi::{LIMITS, assert_valid, errors, refs_ok};

type Pairs = Vec<(String, String)>;

/// One exchange on port 80, owned.
#[derive(Debug, Clone, Default)]
struct Rx {
    method: String,
    path: String,
    status: u16,
    query: String,
    req_headers: Pairs,
    req_body: Option<String>,
    resp_headers: Pairs,
    resp_body: Option<String>,
    host: Option<String>,
    grpc: bool,
}

fn rx(method: &str, path: &str) -> Rx {
    Rx {
        method: method.into(),
        path: path.into(),
        status: 200,
        ..Rx::default()
    }
}

fn pairs(items: &[(&str, &str)]) -> Pairs {
    items
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn record(report: &mut Report, redactor: &Redactor, x: &Rx) {
    let req: Vec<(&str, &str)> = x.req_headers.iter().map(|(n, v)| (&**n, &**v)).collect();
    let resp: Vec<(&str, &str)> = x.resp_headers.iter().map(|(n, v)| (&**n, &**v)).collect();
    report.record(
        &LIMITS,
        redactor,
        &Exchange {
            direction: Direction::Inbound,
            port: 80,
            method: &x.method,
            path: &x.path,
            status: x.status,
            head_ms: 5,
            host: x.host.as_deref(),
            grpc: x.grpc,
            observation: Observation {
                query: &x.query,
                req_headers: &req,
                req_body: x.req_body.as_deref().map(str::as_bytes),
                resp_headers: &resp,
                resp_body: x.resp_body.as_deref().map(str::as_bytes),
            },
        },
    );
}

fn report_of(exchanges: &[Rx]) -> Report {
    let mut r = Report::new(None);
    exchanges
        .iter()
        .for_each(|x| record(&mut r, &Redactor::default(), x));
    r
}

fn doc(report: &Report) -> Value {
    let d = document(report, "ns", "dep", 1, "inbound-80").expect("the document exists");
    assert_valid(&d);
    d
}

/// The operation of `GET /p` (or the given method) of a document.
fn op<'a>(d: &'a Value, path: &str, method: &str) -> &'a Value {
    &d["paths"][path][method]
}

fn parameter<'a>(op: &'a Value, location: &str, name: &str) -> &'a Value {
    op["parameters"]
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|p| p["in"] == location && p["name"] == name)
        })
        .expect("the parameter exists")
}

fn sample_keys(r: &Report) -> Vec<String> {
    r.endpoints()[0]
        .stats
        .samples()
        .iter()
        .map(|(hash, _)| format!("sample-{hash:016x}"))
        .collect()
}

// ---------------------------------------------------------------------------------------
// UI-03

#[test]
fn ui_03_query_required_array_and_order() {
    let r = report_of(&[
        Rx {
            query: "a=1&b=2&b=3".into(),
            req_headers: pairs(&[("x-h", "v")]),
            ..rx("GET", "/p/42")
        },
        Rx {
            query: "a=4&c=5".into(),
            ..rx("GET", "/p/43")
        },
    ]);
    let d = doc(&r);
    let o = op(&d, "/p/{p_id}", "get");
    let order: Vec<(&str, &str)> = o["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .map(|p| {
            (
                p["in"].as_str().expect("in"),
                p["name"].as_str().expect("name"),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("path", "p_id"),
            ("query", "a"),
            ("query", "b"),
            ("query", "c"),
            ("header", "x-h")
        ]
    );
    assert_eq!(parameter(o, "query", "a")["required"], true);
    assert_eq!(parameter(o, "query", "b")["required"], false);
    assert_eq!(parameter(o, "query", "c")["required"], false);
    let b = &parameter(o, "query", "b")["schema"];
    assert_eq!(b["type"], "array");
    assert_eq!(b["items"]["type"], "string");
    assert_eq!(parameter(o, "query", "a")["schema"]["type"], "string");
}

#[test]
fn ui_03_headers_are_in_byte_order_without_the_ignored_ones() {
    let r = report_of(&[Rx {
        req_headers: pairs(&[
            ("X-B", "1"),
            ("Accept", "*/*"),
            ("x-a", "2"),
            ("Content-Type", "text/plain"),
            ("Authorization", "Bearer abc"),
            ("host", "svc"),
        ]),
        ..rx("GET", "/p")
    }]);
    let d = doc(&r);
    let names: Vec<&str> = op(&d, "/p", "get")["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["x-a", "x-b"]);
    let a = parameter(op(&d, "/p", "get"), "header", "x-a");
    assert_eq!(a["required"], true);
}

#[test]
fn ui_03_examples_are_named_after_the_sample_keys() {
    let r = report_of(&[
        Rx {
            query: "a=1".into(),
            ..rx("GET", "/p")
        },
        Rx {
            query: "a=2".into(),
            ..rx("GET", "/p")
        },
        Rx {
            query: "a=1&z=9".into(),
            ..rx("GET", "/p")
        },
    ]);
    let d = doc(&r);
    let a = parameter(op(&d, "/p", "get"), "query", "a");
    let examples = a["examples"].as_object().expect("examples");
    // Three samples, two distinct values: one example per value, named after the first
    // sample key in order that holds it.
    let want: Vec<(String, String)> = {
        let mut seen = Vec::new();
        for (hash, sample) in r.endpoints()[0].stats.samples().iter() {
            let value = sample
                .query()
                .iter()
                .find(|(k, _)| k == "a")
                .map(|(_, v)| v.clone())
                .expect("a");
            if !seen.iter().any(|(_, v)| *v == value) {
                seen.push((format!("sample-{hash:016x}"), value));
            }
        }
        seen
    };
    assert_eq!(want.len(), 2);
    assert_eq!(examples.len(), 2);
    for (name, value) in want {
        assert_eq!(examples[&name], json!({"value": value}), "{name}");
        assert_eq!(name.len(), "sample-".len() + 16);
    }
    // A key that only one sample has has one example.
    let z = parameter(op(&d, "/p", "get"), "query", "z");
    assert_eq!(z["examples"].as_object().expect("examples").len(), 1);
}

#[test]
fn ui_03_volatile_and_redacted_flags() {
    let all: Vec<Rx> = ["r1", "r2", "r3"]
        .iter()
        .map(|id| Rx {
            query: format!("token=secret-{id}&plain=1"),
            req_headers: pairs(&[("x-req-id", id)]),
            ..rx("GET", "/p")
        })
        .collect();
    let d = doc(&report_of(&all));
    let o = op(&d, "/p", "get");
    let volatile = parameter(o, "header", "x-req-id");
    assert_eq!(volatile["x-mimic-volatile"], true);
    assert!(volatile.get("x-mimic-redacted").is_none());
    assert!(volatile["examples"].as_object().expect("examples").len() >= 2);
    let redacted = parameter(o, "query", "token");
    assert_eq!(redacted["x-mimic-redacted"], true);
    let values: Vec<&Value> = redacted["examples"]
        .as_object()
        .expect("examples")
        .values()
        .map(|e| &e["value"])
        .collect();
    assert_eq!(values, [&json!(MARKER)]);
    let plain = parameter(o, "query", "plain");
    assert!(plain.get("x-mimic-volatile").is_none());
    assert!(plain.get("x-mimic-redacted").is_none());
    // The operation lists the fields.
    assert_eq!(o["x-mimic-volatile"], json!(["request.header/x-req-id"]));
    assert!(
        o["x-mimic-redacted"]
            .as_array()
            .expect("redacted")
            .contains(&json!("query/token"))
    );
}

// ---------------------------------------------------------------------------------------
// UI-04

#[test]
fn ui_04_json_request_body() {
    let r = report_of(&[Rx {
        req_headers: pairs(&[("content-type", "application/json")]),
        req_body: Some(r#"{"a":1}"#.into()),
        ..rx("POST", "/p")
    }]);
    let d = doc(&r);
    let body = &op(&d, "/p", "post")["requestBody"];
    assert_eq!(body["required"], true);
    let media = &body["content"]["application/json"];
    assert_eq!(media["schema"], Schema::infer(&json!({"a": 1})).to_json());
    assert_eq!(media["x-mimic-count"], 1);
    assert_eq!(media["x-mimic-bytes"], 7);
    let keys = sample_keys(&r);
    assert_eq!(
        media["examples"],
        json!({ keys[0].clone(): {"value": {"a": 1}} })
    );
}

#[test]
fn ui_04_form_request_body() {
    let r = report_of(&[Rx {
        req_headers: pairs(&[("content-type", "application/x-www-form-urlencoded")]),
        req_body: Some("a=1&a=2&b=x".into()),
        ..rx("POST", "/p")
    }]);
    let d = doc(&r);
    let media =
        &op(&d, "/p", "post")["requestBody"]["content"]["application/x-www-form-urlencoded"];
    assert_eq!(media["schema"]["type"], "object");
    assert_eq!(media["schema"]["properties"]["a"]["type"], "array");
    assert_eq!(media["schema"]["properties"]["b"]["type"], "string");
    let keys = sample_keys(&r);
    assert_eq!(
        media["examples"],
        json!({ keys[0].clone(): {"value": {"a": ["1", "2"], "b": "x"}} })
    );
}

#[test]
fn ui_04_opaque_request_body_has_no_schema() {
    let r = report_of(&[Rx {
        req_headers: pairs(&[("content-type", "text/plain")]),
        req_body: Some("hello".into()),
        ..rx("POST", "/p")
    }]);
    let d = doc(&r);
    let media = &op(&d, "/p", "post")["requestBody"]["content"]["text/plain"];
    assert_eq!(media, &json!({"x-mimic-count": 1, "x-mimic-bytes": 5}));
}

#[test]
fn ui_04_request_body_is_required_when_every_exchange_has_one() {
    let with = Rx {
        req_headers: pairs(&[("content-type", "application/json")]),
        req_body: Some("{}".into()),
        ..rx("POST", "/p")
    };
    let d = doc(&report_of(&[with.clone(), rx("POST", "/p")]));
    let body = &op(&d, "/p", "post")["requestBody"];
    assert!(body.get("required").is_none());
    assert!(body["content"]["application/json"].is_object());
    // No body at all: no requestBody.
    let d = doc(&report_of(&[rx("POST", "/p")]));
    assert!(op(&d, "/p", "post").get("requestBody").is_none());
}

#[test]
fn ui_04_responses_per_status() {
    let mut not_found = rx("GET", "/p");
    not_found.status = 404;
    let mut odd = rx("GET", "/p");
    odd.status = 299;
    odd.resp_headers = pairs(&[("x-odd", "1")]);
    let ok = Rx {
        resp_headers: pairs(&[("content-type", "application/json"), ("x-ok", "yes")]),
        resp_body: Some(r#"{"id":7}"#.into()),
        ..rx("GET", "/p")
    };
    let r = report_of(&[ok, not_found, odd]);
    let d = doc(&r);
    let responses = &op(&d, "/p", "get")["responses"];
    let keys: Vec<&str> = responses
        .as_object()
        .expect("responses")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["200", "299", "404"]);
    assert_eq!(responses["200"]["description"], "OK");
    assert_eq!(responses["404"]["description"], "Not Found");
    assert_eq!(responses["299"]["description"], "Status 299");
    for status in ["200", "299", "404"] {
        assert_eq!(responses[status]["x-mimic-count"], 1, "{status}");
    }
    // A status without a sample: description and count only.
    assert_eq!(
        responses["404"],
        json!({"description": "Not Found", "x-mimic-count": 1})
    );
    // Content and headers come from the status's own samples.
    let content = &responses["200"]["content"]["application/json"];
    assert_eq!(
        content["schema"],
        Schema::infer(&json!({"id": 7})).to_json()
    );
    assert_eq!(content["examples"].as_object().expect("examples").len(), 1);
    let headers: Vec<&str> = responses["200"]["headers"]
        .as_object()
        .expect("headers")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(headers, ["x-ok"]);
    assert_eq!(
        responses["200"]["headers"]["x-ok"]["schema"]["type"],
        "string"
    );
    let odd_headers: Vec<&str> = responses["299"]["headers"]
        .as_object()
        .expect("headers")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(odd_headers, ["x-odd"]);
    assert!(responses["299"].get("content").is_none());
}

#[test]
fn ui_04_opaque_response_media_has_no_schema() {
    let r = report_of(&[Rx {
        resp_headers: pairs(&[("content-type", "text/html; charset=utf-8")]),
        resp_body: Some("<p>".into()),
        ..rx("GET", "/p")
    }]);
    let d = doc(&r);
    let content = &op(&d, "/p", "get")["responses"]["200"]["content"];
    assert_eq!(content, &json!({"text/html": {}}));
}

// ---------------------------------------------------------------------------------------
// UI-05: security

fn authorized(value: &str) -> Rx {
    Rx {
        req_headers: pairs(&[("authorization", value)]),
        ..rx("GET", "/p")
    }
}

#[test]
fn ui_05_security_schemes_from_the_authorization_header() {
    let d = doc(&report_of(&[
        authorized("Bearer abc.def"),
        authorized("Basic dXNlcjpwYXNz"),
    ]));
    assert_eq!(
        d["components"]["securitySchemes"],
        json!({
            "http_basic": {"type": "http", "scheme": "basic"},
            "http_bearer": {"type": "http", "scheme": "bearer"},
        })
    );
    // One requirement per scheme, in byte order of the scheme.
    assert_eq!(
        op(&d, "/p", "get")["security"],
        json!([{"http_basic": []}, {"http_bearer": []}])
    );
}

#[test]
fn ui_05_security_bare_credential_is_an_api_key() {
    let d = doc(&report_of(&[authorized("opaquetoken")]));
    assert_eq!(
        d["components"]["securitySchemes"],
        json!({"authorization": {"type": "apiKey", "in": "header", "name": "Authorization"}})
    );
    assert_eq!(
        op(&d, "/p", "get")["security"],
        json!([{"authorization": []}])
    );
}

#[test]
fn ui_05_security_scheme_outside_the_token_set_is_ignored() {
    // The recorder keeps only alphanumeric schemes, so a hostile report is needed: the scheme
    // of a stored sample is rewritten.
    let json = report_of(&[authorized("Bearer abc")])
        .to_json()
        .expect("serialises");
    for scheme in ["Bea_rer", "Béarer", "Be\\u0020arer"] {
        let forged = json.replace("Bearer <redacted>", &format!("{scheme} <redacted>"));
        let d = doc(&Report::from_json(&forged).expect("parses"));
        assert!(d.get("components").is_none(), "{scheme}");
        assert!(op(&d, "/p", "get").get("security").is_none(), "{scheme}");
    }
}

#[test]
fn ui_05_security_is_per_operation() {
    let mut other = authorized("Basic abc");
    other.path = "/q".into();
    let d = doc(&report_of(&[
        authorized("Bearer abc"),
        other,
        rx("GET", "/r"),
    ]));
    assert_eq!(
        op(&d, "/p", "get")["security"],
        json!([{"http_bearer": []}])
    );
    assert_eq!(op(&d, "/q", "get")["security"], json!([{"http_basic": []}]));
    assert!(op(&d, "/r", "get").get("security").is_none());
    assert_eq!(
        d["components"]["securitySchemes"]
            .as_object()
            .expect("schemes")
            .len(),
        2
    );
}

// ---------------------------------------------------------------------------------------
// UI-01: the `$ref` rule

#[test]
fn ui_01_ref_keys_of_recorded_names_never_reach_a_reference_position() {
    // A media type, a response header and a method named `$ref` are not written as keys.
    let hostile = Rx {
        req_headers: pairs(&[("content-type", "$ref"), ("$ref", "x")]),
        req_body: Some("{}".into()),
        resp_headers: pairs(&[("content-type", "$ref"), ("$ref", "x")]),
        resp_body: Some(r#"{"$ref":"http://h/x","a":{"$ref":1}}"#.into()),
        query: "$ref=1".into(),
        ..rx("$ref", "/p")
    };
    let json_body = Rx {
        req_headers: pairs(&[("content-type", "application/json")]),
        req_body: Some(r#"{"$ref":"http://h/x"}"#.into()),
        ..rx("POST", "/p")
    };
    let d = doc(&report_of(&[hostile, json_body]));
    assert!(refs_ok(&d), "a $ref member outside the allowed places");
    assert!(d["paths"]["/p"].get("x-mimic-methods").is_none());
}

fn arb_rx() -> impl Strategy<Value = Rx> {
    let names = prop::sample::select(vec!["$ref", "a", "x-h", "token", "id"]);
    let media = prop::sample::select(vec![
        "application/json",
        "application/x-www-form-urlencoded",
        "text/plain",
        "$ref",
    ]);
    let body = prop::sample::select(vec![
        r#"{"$ref":"http://h/x"}"#,
        r#"{"a":{"$ref":1,"b":[1,2]}}"#,
        r#"[{"id":1},{"id":2}]"#,
        "$ref=1&a=2&a=3",
        "plain",
    ]);
    (
        prop::sample::select(vec!["GET", "POST", "$ref"]),
        prop::sample::select(vec!["/p", "/p/42", "/$ref"]),
        prop::sample::select(vec![200u16, 201, 500]),
        (names.clone(), names.clone(), any::<u8>()),
        (media.clone(), body.clone(), any::<bool>()),
        (media, body, any::<bool>()),
        prop::sample::select(vec!["Bearer a", "Basic b", "tok", "Bea_rer c"]),
    )
        .prop_map(
            |(method, path, status, (qk, hk, n), (rm, rb, rbody), (sm, sb, sbody), auth)| Rx {
                method: method.into(),
                path: path.into(),
                status,
                query: format!("{qk}=v{}&{qk}=w", n % 3),
                req_headers: pairs(&[
                    (hk, &format!("h{}", n % 4)),
                    ("content-type", rm),
                    ("authorization", auth),
                ]),
                req_body: rbody.then(|| rb.to_string()),
                resp_headers: pairs(&[(hk, "r"), ("content-type", sm)]),
                resp_body: sbody.then(|| sb.to_string()),
                ..Rx::default()
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn ui_01_documents_with_parameters_bodies_and_responses_are_valid(
        exchanges in prop::collection::vec(arb_rx(), 1..8),
        chunk in 1usize..4,
    ) {
        let whole = report_of(&exchanges);
        let parts = exchanges
            .chunks(chunk)
            .map(report_of)
            .rev()
            .reduce(|a, b| merge(a, b, &LIMITS))
            .unwrap();
        let d = document(&whole, "ns", "dep", 1, "inbound-80").unwrap();
        let text = d.to_string();
        let problems = errors(&d);
        prop_assert!(problems.is_empty(), "{:?}", problems);
        prop_assert!(refs_ok(&d));
        // Merging in another grouping and order gives the same bytes (the 404 rule of the
        // recorder aside: no generated status is 404 or 405).
        let again = document(&parts, "ns", "dep", 1, "inbound-80").unwrap();
        prop_assert_eq!(text, again.to_string());
    }
}

// ---------------------------------------------------------------------------------------
// UI-06

/// The string values of the Markdown fields (`description`, `summary`, `title`) of a document,
/// outside example values.
fn markdown_fields(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == "example" || k == "examples" {
                    continue;
                }
                match (k.as_str(), child) {
                    ("description" | "summary" | "title", Value::String(s)) => out.push(s.clone()),
                    _ => markdown_fields(child, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| markdown_fields(i, out)),
        _ => {}
    }
}

/// A canary at every recordable place.
fn canary_exchanges(c: &str) -> Vec<Rx> {
    let json = format!(r#"{{"{c}":"{c}"}}"#);
    vec![
        Rx {
            query: format!("{c}={c}"),
            req_headers: pairs(&[(&format!("x-{c}"), c), ("content-type", "application/json")]),
            req_body: Some(json.clone()),
            resp_headers: pairs(&[(&format!("x-{c}"), c), ("content-type", "application/json")]),
            resp_body: Some(json),
            host: Some(c.to_lowercase()),
            ..rx(c, &format!("/{c}/{c}"))
        },
        Rx {
            req_headers: pairs(&[("content-type", "application/x-www-form-urlencoded")]),
            req_body: Some(format!("{c}={c}&{c}={c}")),
            ..rx("POST", &format!("/{c}"))
        },
        Rx {
            grpc: true,
            ..rx("POST", &format!("/{c}.Svc/{c}"))
        },
        Rx {
            resp_headers: pairs(&[("content-type", c)]),
            resp_body: Some("x".into()),
            ..rx("GET", &format!("/{c}/"))
        },
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn ui_06_no_recorded_content_in_a_markdown_field(canary in "CANARY[A-Za-z0-9]{8,24}") {
        let r = report_of(&canary_exchanges(&canary));
        let ids = super::openapi::index(&r);
        for entry in ids["documents"].as_array().unwrap() {
            let id = entry["id"].as_str().unwrap();
            let d = document(&r, "ns", "dep", 1, id).unwrap();
            let mut fields = Vec::new();
            markdown_fields(&d, &mut fields);
            prop_assert!(!fields.is_empty());
            for field in &fields {
                prop_assert!(!field.contains(&canary));
            }
            prop_assert_eq!(d["info"]["description"].as_str(), Some(DESCRIPTION));
        }
    }
}

const NAME: &str = "xtrafield";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// A canary under a name that only the controller knows (DSC-15) is gone from every
    /// document once the report is re-redacted.
    #[test]
    fn ui_06_no_canary_survives_re_redaction(
        canary in "CANARY[A-Za-z0-9]{8,24}",
        place in 0usize..5,
    ) {
        let mut x = rx("POST", "/p");
        match place {
            0 => x.query = format!("{NAME}={canary}&ok=1"),
            1 => x.req_headers = pairs(&[(&format!("x-{NAME}"), &canary)]),
            2 => x.resp_headers = pairs(&[(&format!("x-{NAME}"), &canary)]),
            3 => {
                x.req_headers = pairs(&[("content-type", "application/x-www-form-urlencoded")]);
                x.req_body = Some(format!("ok=1&{NAME}={canary}"));
            }
            _ => {
                x.req_headers = pairs(&[("content-type", "application/json")]);
                x.req_body = Some(json!({"keep": "v", NAME: {"inner": canary}}).to_string());
            }
        }
        let mut r = Report::new(None);
        for _ in 0..3 {
            record(&mut r, &Redactor::default(), &x);
        }
        prop_assert!(r.to_json().unwrap().contains(&canary));
        r.re_redact(&Redactor::new([NAME]));
        let ids = super::openapi::index(&r);
        for entry in ids["documents"].as_array().unwrap() {
            let d = document(&r, "ns", "dep", 1, entry["id"].as_str().unwrap()).unwrap();
            prop_assert!(!d.to_string().contains(&canary));
        }
        prop_assert!(!super::openapi::endpoint_list(&r).to_string().contains(&canary));
    }
}
