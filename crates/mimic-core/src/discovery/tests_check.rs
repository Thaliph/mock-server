//! CTL-01: the structural check of a pushed report (spec §7.3).

use proptest::prelude::*;
use serde_json::{Value, json};

use super::check::Violation;
use super::observed::{MAX_BODY, Observation};
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report, merge};

const PUSH: usize = 256;

/// A name and the change that makes a valid report break (or not) its bound.
type Case = (&'static str, Box<dyn Fn(&mut Value)>);

fn limits(max_literals: usize, max_endpoints: usize, sample_budget: usize) -> Limits {
    Limits {
        max_literals,
        max_endpoints,
        sample_budget,
    }
}

/// A report with one endpoint `GET /a/{a_id}` that has a JSON response, a sample and a host.
fn valid() -> Value {
    let body = json!({"k": "v"}).to_string();
    let headers = [("content-type", "application/json")];
    let mut r = Report::new(None);
    r.record(
        &limits(32, 256, 1 << 20),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: 80,
            method: "GET",
            path: "/a/1",
            status: 200,
            head_ms: 1,
            host: Some("svc"),
            grpc: false,
            observation: Observation {
                resp_headers: &headers,
                resp_body: Some(body.as_bytes()),
                ..Observation::default()
            },
        },
    );
    serde_json::from_str(&r.to_json().unwrap()).unwrap()
}

const EP: &str = "/scopes/inbound/80/root/literals/a/param/endpoints/GET";

fn at<'a>(v: &'a mut Value, suffix: &str) -> &'a mut Value {
    v.pointer_mut(&format!("{EP}{suffix}"))
        .unwrap_or_else(|| panic!("no {suffix} in the valid report"))
}

fn verdict(v: &Value) -> Result<(), Violation> {
    Report::parse_checked(&v.to_string(), PUSH).map(|_| ())
}

fn schema_leaf() -> Value {
    json!({"kinds": ["String"], "format": "Unseen", "items": null, "object": null})
}

fn schema_object(props: Vec<(String, Value)>) -> Value {
    let required: Vec<&String> = props.iter().map(|(k, _)| k).collect();
    let required = json!(required);
    let props: serde_json::Map<String, Value> = props.into_iter().collect();
    json!({
        "kinds": ["Object"], "format": "Unseen", "items": null,
        "object": {"props": props, "required": required, "map": null}
    })
}

/// `levels` nested arrays around a string leaf: a schema of `levels` levels.
fn schema_chain(levels: usize) -> Value {
    let mut node = schema_leaf();
    for _ in 1..levels {
        node = json!({"kinds": ["Array"], "format": "Unseen", "items": node, "object": null});
    }
    node
}

/// A tree of objects with `fan` properties on each of `levels` levels: many nodes.
fn schema_tree(levels: usize, fan: usize) -> Value {
    if levels == 0 {
        return schema_leaf();
    }
    schema_object(
        (0..fan)
            .map(|i| (format!("k{i}"), schema_tree(levels - 1, fan)))
            .collect(),
    )
}

const JSON_BODY: &str = "/observed/response/bodies/application~1json/json";

fn sample(v: &mut Value) -> &mut Value {
    let samples = at(v, "/samples").as_object_mut().unwrap();
    let key = samples.keys().next().unwrap().clone();
    &mut samples[&key]
}

fn literal_chain(depth: usize, leaf: &str) -> Value {
    let mut node = json!({"literals": {}, "param": null, "absorbing": false,
        "endpoints": {"GET": valid_endpoint()}});
    for i in (0..depth).rev() {
        let name = if i == 0 {
            leaf.to_string()
        } else {
            "s".to_string()
        };
        node =
            json!({"literals": {name: node}, "param": null, "absorbing": false, "endpoints": {}});
    }
    node
}

fn valid_endpoint() -> Value {
    let mut v = valid();
    at(&mut v, "").take()
}

#[test]
fn ctl_01_accepts_the_valid_report() {
    assert_eq!(verdict(&valid()), Ok(()));
}

#[test]
fn ctl_01_rejects_each_violation_with_its_own_reason() {
    let long_name = "n".repeat(129);
    let cases: Vec<Case> = vec![
        ("version", Box::new(|v| v["version"] = json!(2))),
        (
            "samples",
            Box::new(|v| {
                let one = sample(v).clone();
                let all = at(v, "/samples").as_object_mut().unwrap();
                for i in 0..20 {
                    all.insert(format!("{}", 1000 + i), one.clone());
                }
            }),
        ),
        (
            "hosts",
            Box::new(|v| {
                *at(v, "/hosts") = json!((0..17).map(|i| format!("h{i}")).collect::<Vec<_>>());
            }),
        ),
        (
            "host_length",
            Box::new(|v| *at(v, "/hosts") = json!(["h".repeat(1025)])),
        ),
        (
            "status",
            Box::new(|v| *at(v, "/statuses") = json!({"99": 1})),
        ),
        (
            "status",
            Box::new(|v| *at(v, "/statuses") = json!({"600": 1})),
        ),
        (
            "status",
            Box::new(|v| sample(v)["response"]["status"] = json!(600)),
        ),
        (
            "bucket",
            Box::new(|v| *at(v, "/latency/buckets") = json!({"70": 1})),
        ),
        (
            "schema_nodes",
            Box::new(|v| *at(v, JSON_BODY) = schema_tree(3, 8)),
        ),
        (
            "schema_levels",
            Box::new(|v| *at(v, JSON_BODY) = schema_chain(17)),
        ),
        (
            "schema_cut",
            Box::new(|v| at(v, JSON_BODY)["cut"] = json!(17)),
        ),
        (
            "name_length",
            Box::new({
                let name = long_name.clone();
                move |v| *at(v, JSON_BODY) = schema_object(vec![(name.clone(), schema_leaf())])
            }),
        ),
        (
            "name_length",
            Box::new({
                let name = long_name.clone();
                move |v| {
                    let key = at(v, "/observed/query/keys").as_object_mut().unwrap();
                    key.insert(
                        name.clone(),
                        json!({"count": 1, "array": false, "schema": schema_leaf()}),
                    );
                }
            }),
        ),
        (
            "params",
            Box::new(|v| {
                let keys = at(v, "/observed/query/keys").as_object_mut().unwrap();
                for i in 0..65 {
                    keys.insert(
                        format!("q{i}"),
                        json!({"count": 1, "array": false, "schema": schema_leaf()}),
                    );
                }
            }),
        ),
        (
            "params",
            Box::new(|v| {
                let keys = at(v, "/observed/request/headers/keys")
                    .as_object_mut()
                    .unwrap();
                for i in 0..65 {
                    keys.insert(
                        format!("h{i}"),
                        json!({"count": 1, "array": false, "schema": schema_leaf()}),
                    );
                }
            }),
        ),
        (
            "volatile_fields",
            Box::new(|v| {
                let fields = at(v, "/observed/fields/fields").as_object_mut().unwrap();
                for i in 0..129 {
                    fields.insert(
                        format!("f{i}"),
                        json!({"n": 1, "redacted": false, "sketch": {"hashes": [1]}}),
                    );
                }
            }),
        ),
        (
            "sketch",
            Box::new(|v| {
                let fields = at(v, "/observed/fields/fields").as_object_mut().unwrap();
                fields.insert(
                    "wide".into(),
                    json!({"n": 1, "redacted": false, "sketch": {"hashes": (0..33).collect::<Vec<u64>>()}}),
                );
            }),
        ),
        (
            "path_length",
            Box::new(|v| {
                v["scopes"]["inbound"]["80"]["root"] = literal_chain(1, &"p".repeat(2048));
            }),
        ),
        (
            "path_length",
            Box::new(|v| {
                v["scopes"]["inbound"]["80"]["raw"] =
                    json!({ format!("/{}", "p".repeat(2048)): {"GET": valid_endpoint()} });
            }),
        ),
        (
            "path_segments",
            Box::new(|v| v["scopes"]["inbound"]["80"]["root"] = literal_chain(33, "x")),
        ),
        (
            "path_segments",
            Box::new(|v| {
                let path = format!("/{}", vec!["s"; 33].join("/"));
                v["scopes"]["inbound"]["80"]["raw"] = json!({ path: {"GET": valid_endpoint()} });
            }),
        ),
        (
            "media_types",
            Box::new(|v| {
                let bodies = at(v, "/observed/response/bodies").as_object_mut().unwrap();
                for i in 0..16 {
                    bodies.insert(
                        format!("application/x-{i}"),
                        json!({"count": 1, "bytes": 1, "json": null, "form": null}),
                    );
                }
            }),
        ),
        (
            "body_size",
            Box::new(|v| {
                // Serialised, the string is MAX_BODY + 1 bytes with its quotes.
                sample(v)["response"]["body"]["value"] = json!("x".repeat(MAX_BODY - 1));
            }),
        ),
        (
            "body_media",
            Box::new(|v| {
                sample(v)["response"]["body"]["media"] = json!("application/octet-stream");
            }),
        ),
        ("unknown_field", Box::new(|v| v["extra"] = json!(1))),
        (
            "unknown_field",
            Box::new(|v| at(v, "")["extra"] = json!({"a": 1})),
        ),
        (
            "unknown_field",
            Box::new(|v| sample(v)["request"]["extra"] = json!(true)),
        ),
    ];
    for (reason, mutate) in cases {
        let mut v = valid();
        mutate(&mut v);
        let got = verdict(&v).expect_err(reason);
        assert_eq!(got.reason(), reason, "{reason}");
        assert!(!got.is_push_limit(), "{reason}");
    }
}

#[test]
fn ctl_01_accepts_the_bounds_exactly() {
    let cases: Vec<Case> = vec![
        (
            "20 samples",
            Box::new(|v| {
                let one = sample(v).clone();
                let all = at(v, "/samples").as_object_mut().unwrap();
                for i in 0..19 {
                    all.insert(format!("{}", 1000 + i), one.clone());
                }
            }),
        ),
        (
            "16 hosts of 1024 bytes",
            Box::new(|v| {
                *at(v, "/hosts") = json!(
                    (0..16)
                        .map(|i| format!("{i:x}{}", "h".repeat(1023)))
                        .collect::<Vec<_>>()
                );
            }),
        ),
        (
            "statuses 100 and 599",
            Box::new(|v| *at(v, "/statuses") = json!({"100": 1, "599": 1})),
        ),
        (
            "overflow bucket",
            Box::new(|v| *at(v, "/latency/buckets") = json!({"0": 1, "69": 1})),
        ),
        (
            "512 nodes",
            Box::new(|v| {
                // 1 + 7 * 73 = 512.
                let props = (0..7)
                    .map(|i| (format!("k{i}"), schema_tree(2, 8)))
                    .collect();
                let root = schema_object(props);
                *at(v, JSON_BODY) = root;
            }),
        ),
        (
            "16 levels and cut 16",
            Box::new(|v| {
                let mut s = schema_chain(16);
                s["cut"] = json!(16);
                *at(v, JSON_BODY) = s;
            }),
        ),
        (
            "128-byte name",
            Box::new(|v| {
                *at(v, JSON_BODY) = schema_object(vec![("n".repeat(128), schema_leaf())]);
            }),
        ),
        (
            "32 segments",
            Box::new(|v| v["scopes"]["inbound"]["80"]["root"] = literal_chain(32, "x")),
        ),
        (
            "2048-byte path",
            Box::new(|v| {
                v["scopes"]["inbound"]["80"]["root"] = literal_chain(1, &"p".repeat(2047));
            }),
        ),
        (
            "body of MAX_BODY bytes",
            Box::new(|v| {
                sample(v)["response"]["body"]["value"] = json!("x".repeat(MAX_BODY - 2));
            }),
        ),
    ];
    for (what, mutate) in cases {
        let mut v = valid();
        mutate(&mut v);
        assert_eq!(verdict(&v), Ok(()), "{what}");
    }
}

#[test]
fn ctl_01_push_limit_is_per_direction() {
    let l = limits(100, 256, 1 << 20);
    let record = |r: &mut Report, direction, path: &str| {
        r.record(
            &l,
            &Redactor::default(),
            &Exchange {
                direction,
                port: 80,
                method: "GET",
                path,
                status: 200,
                head_ms: 1,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
    };
    let mut r = Report::new(None);
    for p in ["/a", "/b", "/c"] {
        record(&mut r, Direction::Inbound, p);
        record(&mut r, Direction::Outbound, p);
    }
    assert_eq!(r.check(3), Ok(()));
    record(&mut r, Direction::Outbound, "/d");
    let err = r.check(3).unwrap_err();
    assert_eq!(err, Violation::PushLimit);
    assert_eq!(err.reason(), "push_limit");
    assert!(err.is_push_limit());
    let err = Report::parse_checked(&r.to_json().unwrap(), 3).unwrap_err();
    assert_eq!(err.reason(), "push_limit");
}

#[test]
fn ctl_01_malformed_input_is_a_violation_not_a_panic() {
    for bad in [
        "",
        "{",
        "null",
        "[]",
        "{\"version\":1}",
        "\u{0}",
        "{\"version\":\"1\"}",
    ] {
        let err = Report::parse_checked(bad, PUSH).unwrap_err();
        assert!(
            matches!(err, Violation::Malformed | Violation::Version),
            "{bad:?}: {err:?}"
        );
    }
    let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(Report::parse_checked(&deep, PUSH).is_err());
}

#[test]
fn ctl_01_reasons_are_fixed_and_distinct() {
    let all = [
        Violation::Malformed,
        Violation::UnknownField,
        Violation::Version,
        Violation::PushLimit,
        Violation::Samples,
        Violation::Hosts,
        Violation::HostLength,
        Violation::Status,
        Violation::Bucket,
        Violation::SchemaNodes,
        Violation::SchemaLevels,
        Violation::SchemaCut,
        Violation::NameLength,
        Violation::Params,
        Violation::VolatileFields,
        Violation::Sketch,
        Violation::PathLength,
        Violation::PathSegments,
        Violation::MediaTypes,
        Violation::BodySize,
        Violation::BodyMedia,
    ];
    let reasons: std::collections::BTreeSet<_> = all.iter().map(|v| v.reason()).collect();
    assert_eq!(reasons.len(), all.len());
    assert!(
        reasons
            .iter()
            .all(|r| r.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    );
}

/// One recorded exchange, owned.
#[derive(Debug, Clone)]
struct Ex {
    outbound: bool,
    port: u16,
    path: String,
    status: u16,
    ms: u64,
    host: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    grpc: bool,
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        "[a-z0-9 ]{0,12}".prop_map(Value::String),
        "[A-Za-z0-9]{16,40}".prop_map(Value::String),
    ];
    leaf.prop_recursive(20, 40, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::btree_map("[a-z]{1,200}", inner.clone(), 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
            // Wide objects, which become maps.
            (130usize..160, any::<i64>()).prop_map(|(n, v)| Value::Object(
                (0..n).map(|i| (format!("w{i}"), json!(v))).collect()
            )),
        ]
    })
}

fn path() -> impl Strategy<Value = String> {
    let seg = prop_oneof![
        8 => prop_oneof![
            Just("a".to_string()), Just("b".to_string()), Just("users".to_string()),
            Just("42".to_string()), Just("x-y".to_string()), Just("".to_string()),
        ],
        1 => "[a-z]{1,300}",
        1 => "[0-9a-f]{8,40}",
    ];
    proptest::collection::vec(seg, 1..40).prop_map(|s| format!("/{}", s.join("/")))
}

/// Mostly a few pairs, sometimes enough to pass the 64-key bound.
fn pairs(key: &'static str) -> impl Strategy<Value = Vec<(String, String)>> {
    prop_oneof![
        6 => proptest::collection::vec((key, "[a-z0-9]{0,8}"), 0..3),
        1 => proptest::collection::vec((key, "[a-z0-9]{0,8}"), 60..70),
    ]
}

fn exchange() -> impl Strategy<Value = Ex> {
    (
        (
            any::<bool>(),
            prop_oneof![Just(80u16), Just(8080)],
            path(),
            prop_oneof![
                8 => Just(200u16),
                3 => 100u16..600,
                1 => Just(99u16),
                1 => Just(600u16),
            ],
            prop_oneof![0u64..5, 0u64..3000, 100_000u64..300_000, Just(u64::MAX)],
        ),
        (
            "[a-z0-9.]{0,1024}",
            pairs("[a-z]{1,200}"),
            pairs("x-[a-z]{1,150}"),
            proptest::option::of(json_value()),
            any::<bool>(),
        ),
    )
        .prop_map(
            |((outbound, port, path, status, ms), (host, query, headers, body, grpc))| Ex {
                outbound,
                port,
                path,
                status,
                ms,
                host,
                query: query
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("&"),
                headers,
                body: body.map(|b| b.to_string()),
                grpc,
            },
        )
}

fn record_all(l: &Limits, items: &[Ex]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for e in items {
        let mut headers: Vec<(&str, &str)> = e
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        headers.push(("content-type", "application/json"));
        r.record(
            l,
            &Redactor::default(),
            &Exchange {
                direction: if e.outbound {
                    Direction::Outbound
                } else {
                    Direction::Inbound
                },
                port: e.port,
                method: "POST",
                path: &e.path,
                status: e.status,
                head_ms: e.ms,
                host: Some(&e.host),
                grpc: e.grpc,
                observation: Observation {
                    query: &e.query,
                    req_headers: &headers,
                    req_body: e.body.as_deref().map(str::as_bytes),
                    resp_headers: &headers,
                    resp_body: e.body.as_deref().map(str::as_bytes),
                },
            },
        );
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn ctl_01_accepts_what_the_recorder_produces(
        items in proptest::collection::vec(exchange(), 1..6),
        split in 0usize..8,
        max_literals in 1usize..5,
        max_endpoints in 1usize..12,
        budget in prop_oneof![Just(4_000usize), Just(1usize << 20), Just(16usize << 20)],
    ) {
        let l = limits(max_literals, max_endpoints, budget);
        let split = split.min(items.len());
        let a = record_all(&l, &items[..split]);
        let b = record_all(&l, &items[split..]);
        for r in [&a, &b] {
            prop_assert_eq!(r.check(PUSH), Ok(()), "recorded");
            let json = r.to_json().unwrap();
            prop_assert!(Report::parse_checked(&json, PUSH).is_ok(), "round trip");
        }
        let merged = merge(a, b, &l);
        prop_assert_eq!(merged.check(PUSH), Ok(()), "merged");
        prop_assert!(Report::parse_checked(&merged.to_json().unwrap(), PUSH).is_ok());
    }

    #[test]
    fn ctl_01_never_panics_on_arbitrary_input(s in "\\PC{0,400}") {
        let _ = Report::parse_checked(&s, PUSH);
    }

    #[test]
    fn ctl_01_never_panics_on_corrupted_reports(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        cut in proptest::option::of(any::<prop::sample::Index>()),
    ) {
        let mut bytes = valid().to_string().into_bytes();
        for (at, b) in edits {
            let i = at.index(bytes.len());
            bytes[i] = b;
        }
        if let Some(c) = cut {
            bytes.truncate(c.index(bytes.len()));
        }
        let _ = Report::parse_checked(&String::from_utf8_lossy(&bytes), PUSH);
    }
}
