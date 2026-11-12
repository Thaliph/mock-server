//! EGR-06, EGR-07: the outbound view of redaction (spec §6.1), in the recorder and in the
//! controller's re-redaction. Canaries never appear in assertion messages.

use std::net::SocketAddr;

use proptest::prelude::*;
use serde_json::Value;
use xxhash_rust::xxh3::xxh3_64;

use super::observed::Observation;
use super::redact::{MARKER, Redactor, Rule, value_rule};
use super::report::{Direction, Exchange, Limits, Report, Via};
use super::scope::{ScopeKey, authority_key};

const AUTHORITY: &str = "api.example.com:443";

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

const PLAIN: Via<'static> = Via {
    tls: false,
    sni: None,
    dst: None,
};

/// A 35-character body of an `AIza` key.
fn aiza_tail() -> String {
    "Xy-_0".repeat(7)
}

fn record(direction: Direction, o: Observation<'_>) -> Report {
    let mut r = Report::new(None);
    let e = Exchange {
        direction,
        port: 8080,
        method: "POST",
        path: "/x",
        status: 200,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: o,
    };
    match direction {
        Direction::Outbound => {
            r.record_at(
                ScopeKey::Authority(AUTHORITY.into()),
                &PLAIN,
                &limits(),
                &Redactor::default(),
                &e,
            );
        }
        Direction::Inbound => {
            r.record(&limits(), &Redactor::default(), &e);
        }
    }
    r
}

fn json(r: &Report) -> Value {
    serde_json::from_str(&r.to_json().unwrap()).unwrap()
}

type Pairs = Vec<(String, String)>;

fn sample_of(r: &Report) -> (Pairs, Pairs, Pairs) {
    let e = &r.endpoints()[0];
    let (_, s) = e.stats.samples().iter().next().unwrap();
    (
        s.request_headers().to_vec(),
        s.query().to_vec(),
        s.response_headers().to_vec(),
    )
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
    h.iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

const CANARY: &str = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4d6";

#[test]
fn egr_06_outbound_request_headers_keep_only_the_allowlist() {
    let headers = [
        ("Ocp-Apim-Subscription-Key", CANARY),
        ("X-Tenant-Key", CANARY),
        ("X-Anything", CANARY),
        ("Accept", "application/json"),
        ("Content-Type", "text/plain"),
        ("User-Agent", "app/1.0"),
        ("If-None-Match", "\"abc\""),
        ("Authorization", &format!("Basic {CANARY}")),
        ("Cookie", &format!("a={CANARY}")),
    ];
    let r = record(
        Direction::Outbound,
        Observation {
            req_headers: &headers,
            ..Observation::default()
        },
    );
    let (req, _, _) = sample_of(&r);
    for name in ["ocp-apim-subscription-key", "x-tenant-key", "x-anything"] {
        assert_eq!(header(&req, name), "mimic-secret");
    }
    assert_eq!(header(&req, "accept"), "application/json");
    assert_eq!(header(&req, "content-type"), "text/plain");
    assert_eq!(header(&req, "user-agent"), "app/1.0");
    assert_eq!(header(&req, "if-none-match"), "\"abc\"");
    assert_eq!(header(&req, "authorization"), "Basic mimic-auth");
    assert_eq!(header(&req, "cookie"), "a=mimic-cookie");
    assert!(!r.to_json().unwrap().contains(CANARY));
    let v = json(&r);
    let fields = &v["scopes"]["outbound"][AUTHORITY]["root"]["literals"]["x"]["endpoints"]["POST"]
        ["observed"]["fields"]["fields"];
    for name in ["x-tenant-key", "x-anything", "ocp-apim-subscription-key"] {
        assert_eq!(fields[format!("request.header/{name}")]["redacted"], true);
    }
    assert_eq!(fields["request.header/accept"]["redacted"], false);
}

#[test]
fn egr_06_the_rule_is_outbound_header_and_names_keep_their_rules() {
    let out = Redactor::default().outbound_request();
    assert_eq!(
        out.header_rules("X-Tenant-Key", "v"),
        [Rule::OutboundHeader]
    );
    assert_eq!(out.header_rules("x-anything", "v"), [Rule::OutboundHeader]);
    assert_eq!(Rule::OutboundHeader.as_str(), "outbound_header");
    assert!(out.header_rules("accept", "text/html").is_empty());
    assert_eq!(out.header_rules("authorization", "Bearer x"), [Rule::Name]);
    assert_eq!(out.header("authorization", "Frobnicate x"), "mimic-auth");
    assert_eq!(
        out.header("proxy-authorization", "Digest x"),
        "Digest mimic-auth"
    );
    // The view is for requests only.
    let plain = Redactor::default();
    assert_eq!(plain.header("x-anything", "v"), "v");
}

#[test]
fn egr_06_key_and_sig_are_redacted_by_name_in_outbound_requests_only() {
    let body = format!(r#"{{"idempotency_key":"{CANARY}","ok":"v","n":{{"sig":"{CANARY}"}}}}"#);
    let headers = [("content-type", "application/json")];
    let query = format!("key={CANARY}&sig={CANARY}&ok=v");
    let r = record(
        Direction::Outbound,
        Observation {
            query: &query,
            req_headers: &headers,
            req_body: Some(body.as_bytes()),
            ..Observation::default()
        },
    );
    assert!(!r.to_json().unwrap().contains(CANARY));
    let (_, q, _) = sample_of(&r);
    assert_eq!(q.iter().find(|(k, _)| k == "ok").unwrap().1, "v");
    // Kept in an inbound request, and in an outbound response.
    let inbound = record(
        Direction::Inbound,
        Observation {
            query: "key=v&sig=w",
            ..Observation::default()
        },
    );
    let (_, q, _) = sample_of(&inbound);
    assert_eq!(q.iter().find(|(k, _)| k == "key").unwrap().1, "v");
    assert_eq!(q.iter().find(|(k, _)| k == "sig").unwrap().1, "w");
    let resp_headers = [("content-type", "application/json"), ("x-anything", "v")];
    let resp_body = br#"{"key":"v","sig":"w"}"#;
    let outbound = record(
        Direction::Outbound,
        Observation {
            resp_headers: &resp_headers,
            resp_body: Some(resp_body),
            resp_trailers: &[],
            ..Observation::default()
        },
    );
    let (_, _, resp) = sample_of(&outbound);
    assert_eq!(header(&resp, "x-anything"), "v");
    let (_, s) = outbound.endpoints()[0]
        .stats
        .samples()
        .iter()
        .next()
        .map(|(h, s)| (h, s.clone()))
        .unwrap();
    let (_, body) = s.response_body().unwrap();
    assert_eq!(body["key"], "v");
    assert_eq!(body["sig"], "w");
    let (_, s) = outbound.endpoints()[0]
        .stats
        .samples()
        .iter()
        .next()
        .map(|(h, s)| (h, s.clone()))
        .unwrap();
    assert_eq!(s.request_headers().len(), 0);
}

#[test]
fn egr_06_a_google_api_key_is_redacted_by_value_in_both_directions() {
    let key = format!("AIza{}", aiza_tail());
    assert_eq!(value_rule(&key), Some(Rule::TokenPrefix));
    assert_eq!(value_rule(&key[..key.len() - 1]), None);
    assert_eq!(value_rule(&format!("{key}x")), None);
    for direction in [Direction::Inbound, Direction::Outbound] {
        let query = format!("q={key}&short={}", &key[..key.len() - 1]);
        let r = record(
            direction,
            Observation {
                query: &query,
                ..Observation::default()
            },
        );
        let (_, q, _) = sample_of(&r);
        assert_eq!(q.iter().find(|(k, _)| k == "q").unwrap().1, "mimic-api-key");
        assert_eq!(
            q.iter().find(|(k, _)| k == "short").unwrap().1,
            key[..key.len() - 1]
        );
    }
}

#[test]
fn egr_06_a_mixed_case_secret_in_a_host_is_caught_on_the_original_text() {
    let d: SocketAddr = "10.0.0.5:443".parse().unwrap();
    let aiza = format!("AIza{}", aiza_tail());
    assert_eq!(
        authority_key(Some(&format!("{aiza}:443")), 443, None, d),
        format!("{MARKER}:443")
    );
    assert_eq!(
        authority_key(None, 443, Some(&aiza), d),
        format!("{MARKER}:443")
    );
    // Lower-casing is how DNS compares names: a prefix in another case is a secret too.
    assert_eq!(
        authority_key(Some("SK-Live1234567890:443"), 443, None, d),
        format!("{MARKER}:443")
    );
    // No part of it reaches the origin.
    let mut r = Report::new(None);
    r.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &Via {
            tls: true,
            sni: Some(&aiza),
            dst: Some(d),
        },
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 0,
            method: "GET",
            path: "/x",
            status: 200,
            head_ms: 1,
            host: None,
            grpc: false,
            observation: Observation::default(),
        },
    );
    let text = r.to_json().unwrap();
    assert!(!text.contains(&aiza));
    assert!(
        !text
            .to_ascii_lowercase()
            .contains(&aiza.to_ascii_lowercase())
    );
}

#[test]
fn egr_06_re_redaction_applies_the_outbound_view() {
    let headers = [("x-custom", "v"), ("accept", "a/b")];
    let query = "key=v&ok=w";
    let r = record(
        Direction::Outbound,
        Observation {
            query,
            req_headers: &headers,
            ..Observation::default()
        },
    );
    // A report as a pod that skipped the view would have pushed it: the markers replaced.
    let text = r.to_json().unwrap();
    let leaked = text
        .replace(
            r#"["x-custom","mimic-secret"]"#,
            &format!(r#"["x-custom","{CANARY}"]"#),
        )
        .replace(
            r#"["key","mimic-api-key"]"#,
            &format!(r#"["key","{CANARY}"]"#),
        );
    assert!(leaked.contains(CANARY));
    let mut back = Report::from_json(&leaked).unwrap();
    back.re_redact(&Redactor::default());
    let after = back.to_json().unwrap();
    assert!(!after.contains(CANARY));
    let (req, q, _) = sample_of(&back);
    assert_eq!(header(&req, "x-custom"), "mimic-secret");
    assert_eq!(header(&req, "accept"), "a/b");
    assert_eq!(q.iter().find(|(k, _)| k == "ok").unwrap().1, "w");
    // The same text as an inbound request is not touched.
    let inbound = record(
        Direction::Inbound,
        Observation {
            query,
            req_headers: &headers,
            ..Observation::default()
        },
    );
    let mut again = Report::from_json(&inbound.to_json().unwrap()).unwrap();
    again.re_redact(&Redactor::default());
    assert_eq!(again, inbound);
}

#[test]
fn egr_06_removing_endpoints_keeps_the_version_in_step() {
    let mut r = Report::new(None);
    r.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &PLAIN,
        &Limits {
            max_authorities: 1,
            ..limits()
        },
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 0,
            method: "GET",
            path: "/x",
            status: 200,
            head_ms: 1,
            host: None,
            grpc: false,
            observation: Observation::default(),
        },
    );
    assert!(r.to_json().unwrap().starts_with("{\"version\":2,"));
    let merged = super::report::merge(
        Report::new(None),
        Report::from_json(&r.to_json().unwrap()).unwrap(),
        &Limits {
            max_authorities: 1,
            ..limits()
        },
    );
    assert!(merged.to_json().unwrap().starts_with("{\"version\":2,"));
}

// ---- EGR-07 ----

#[derive(Debug, Clone)]
enum Place {
    Path,
    Query,
    HeaderValue,
    FormField,
    JsonKey,
    JsonValue,
    ResponseHeader,
    ResponseJson,
}

#[derive(Debug, Clone)]
enum Form {
    /// The canary is the value of a listed name.
    Listed(&'static str),
    /// The canary sits inside a form a value detector catches.
    Detected(&'static str),
    /// The value of a request header with an arbitrary name outside the allowlist.
    Header(String),
}

const ALLOWED: [&str; 20] = [
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-encoding",
    "content-language",
    "content-type",
    "expect",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "pragma",
    "range",
    "user-agent",
    "grpc-accept-encoding",
    "grpc-encoding",
    "grpc-timeout",
];

fn form() -> impl Strategy<Value = Form> {
    prop_oneof![
        prop_oneof![
            Just("key"),
            Just("sig"),
            Just("idempotency_key"),
            Just("password"),
            Just("access_token"),
            Just("client_secret"),
            Just("apiKey"),
        ]
        .prop_map(Form::Listed),
        prop_oneof![
            Just("Bearer "),
            Just("Basic "),
            Just("sk_live_"),
            Just("ghp_"),
            Just("AIza"),
        ]
        .prop_map(Form::Detected),
        "[a-z][a-z0-9-]{0,14}"
            .prop_filter("outside the allowlist", |n| !ALLOWED.contains(&n.as_str()))
            .prop_map(Form::Header),
    ]
}

fn place() -> impl Strategy<Value = Place> {
    prop_oneof![
        Just(Place::Path),
        Just(Place::Query),
        Just(Place::HeaderValue),
        Just(Place::FormField),
        Just(Place::JsonKey),
        Just(Place::JsonValue),
        Just(Place::ResponseHeader),
        Just(Place::ResponseJson),
    ]
}

/// The canary as the value, per form: `AIza` + 35 characters is the key itself.
fn secret(form: &Form, canary: &str) -> String {
    match form {
        Form::Detected("AIza") => format!("AIza{canary}"),
        Form::Detected(prefix) => format!("{prefix}{canary}"),
        _ => canary.to_string(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn egr_07_no_canary_in_an_outbound_report(
        canary in "[A-Za-z0-9]{35}",
        placed in proptest::collection::vec((place(), form(), 0usize..4), 1..4),
    ) {
        let mut path = String::from("/v1/items");
        let mut query = String::from("ok=1");
        let mut req_headers: Vec<(String, String)> = Vec::new();
        let mut form_body: Vec<String> = Vec::new();
        let mut json_req = serde_json::Map::new();
        let mut resp_headers: Vec<(String, String)> = Vec::new();
        let mut json_resp = serde_json::Map::new();
        for (place, form, depth) in &placed {
            // A header name outside the allowlist protects header values only.
            let place = if matches!(form, Form::Header(_)) {
                &Place::HeaderValue
            } else {
                place
            };
            // `key` and `sig` are redacted by name in requests only.
            let request_only = matches!(form, Form::Listed("key" | "sig" | "idempotency_key"));
            if request_only && matches!(place, Place::ResponseHeader | Place::ResponseJson) {
                continue;
            }
            let value = secret(form, &canary);
            let name = match form {
                Form::Listed(n) => (*n).to_string(),
                Form::Detected(_) => "plain".to_string(),
                Form::Header(n) => n.clone(),
            };
            let nest = |leaf: Value| (0..*depth).fold(leaf, |v, i| {
                let mut m = serde_json::Map::new();
                m.insert(format!("n{i}"), v);
                Value::Object(m)
            });
            match place {
                Place::Path => {
                    if matches!(form, Form::Detected(_)) {
                        path = format!("/v1/{}", value.replace(' ', "%20"));
                    }
                }
                Place::Query => query.push_str(&format!("&{name}={}", value.replace(' ', "%20"))),
                Place::HeaderValue => {
                    // A request header, whatever its name outside the allowlist.
                    let header_name = match form {
                        Form::Header(n) => n.clone(),
                        _ => "x-extra".to_string(),
                    };
                    req_headers.push((header_name, value));
                }
                Place::FormField => form_body.push(format!("{name}={}", value.replace(' ', "%20"))),
                Place::JsonKey | Place::JsonValue => {
                    json_req.insert(name, nest(Value::String(value)));
                }
                Place::ResponseHeader => resp_headers.push((name, value)),
                Place::ResponseJson => {
                    json_resp.insert(name, nest(Value::String(value)));
                }
            }
        }
        let json_body = serde_json::to_vec(&Value::Object(json_req)).unwrap();
        let resp_body = serde_json::to_vec(&Value::Object(json_resp)).unwrap();
        let form_text = form_body.join("&");
        let mut hs: Vec<(&str, &str)> = req_headers.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let rs: Vec<(&str, &str)> = resp_headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .chain([("content-type", "application/json")])
            .collect();
        // One report per request body kind: JSON, then form.
        for (media, body) in [
            ("application/json", json_body.as_slice()),
            ("application/x-www-form-urlencoded", form_text.as_bytes()),
        ] {
            hs.push(("content-type", media));
            let r = record(
                Direction::Outbound,
                Observation {
                    query: &query,
                    req_headers: &hs,
                    req_body: Some(body),
                    resp_headers: &rs,
                    resp_body: Some(&resp_body),
                    resp_trailers: &[],
                },
            );
            hs.pop();
            let mut path_report = Report::new(None);
            path_report.record_at(
                ScopeKey::Authority(AUTHORITY.into()),
                &PLAIN,
                &limits(),
                &Redactor::default(),
                &Exchange {
                    direction: Direction::Outbound,
                    port: 0,
                    method: "GET",
                    path: &path,
                    status: 200,
                    head_ms: 1,
                    host: None,
                    grpc: false,
                    observation: Observation::default(),
                },
            );
            for report in [r, path_report] {
                let text = report.to_json().unwrap();
                let hash = xxh3_64(canary.as_bytes());
                let hex = format!("{hash:x}");
                prop_assert!(!text.contains(&canary));
                prop_assert!(!text.contains(&hash.to_string()));
                prop_assert!(!text.contains(&hex));
            }
        }
    }
}
