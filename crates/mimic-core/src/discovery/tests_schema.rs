use proptest::prelude::*;
use serde_json::{Value, json};

use super::observed::{Observation, Observed};
use super::redact::Redactor;
use super::schema::{Params, Schema};
use super::volatile::{Sketch, Volatility};

const UUID: &str = "123e4567-e89b-12d3-a456-426614174000";
const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";

fn schema(v: Value) -> Value {
    Schema::infer(&v).to_json()
}

fn merged(values: &[Value]) -> Value {
    values
        .iter()
        .map(Schema::infer)
        .reduce(Schema::merge)
        .unwrap()
        .to_json()
}

fn redactor() -> Redactor {
    Redactor::new([])
}

fn query(qs: &[&str]) -> Value {
    qs.iter()
        .map(|q| Params::from_query(&redactor(), q))
        .reduce(Params::merge)
        .unwrap()
        .to_json()
}

fn observed(o: &Observation<'_>) -> Observed {
    let mut obs = Observed::default();
    obs.record(&redactor(), o);
    obs
}

fn nested(levels: usize) -> Value {
    (0..levels).fold(json!(1), |inner, _| json!({ "a": inner }))
}

mod schemas {
    use super::*;

    #[test]
    fn dsc_03_types_are_a_set_with_null() {
        assert_eq!(schema(json!("x")), json!({"type": "string"}));
        assert_eq!(
            merged(&[json!(null), json!("x")]),
            json!({"type": ["null", "string"]})
        );
        assert_eq!(
            merged(&[json!(true), json!({"a": 1}), json!([1])])["type"],
            json!(["array", "boolean", "object"])
        );
    }

    #[test]
    fn dsc_03_integer_only_while_every_number_is_integral() {
        assert_eq!(merged(&[json!(1), json!(-7)]), json!({"type": "integer"}));
        assert_eq!(merged(&[json!(1), json!(2.0)]), json!({"type": "integer"}));
        assert_eq!(merged(&[json!(1), json!(1.5)]), json!({"type": "number"}));
        assert_eq!(merged(&[json!(1.5), json!(1)]), json!({"type": "number"}));
    }

    #[test]
    fn dsc_03_format_only_when_every_value_has_it() {
        for (value, format) in [
            (UUID, "uuid"),
            ("2024-05-06T07:08:09Z", "date-time"),
            ("2024-05-06", "date"),
            ("a@example.com", "email"),
            ("https://example.com/x", "uri"),
            ("10.0.0.1", "ipv4"),
            ("::1", "ipv6"),
        ] {
            assert_eq!(
                schema(json!(value)),
                json!({"type": "string", "format": format}),
                "{value}"
            );
        }
        assert_eq!(
            merged(&[json!(UUID), json!("123e4567-e89b-12d3-a456-426614174001")]),
            json!({"type": "string", "format": "uuid"})
        );
        assert_eq!(
            merged(&[json!(UUID), json!("plain")]),
            json!({"type": "string"})
        );
        assert_eq!(
            merged(&[json!("plain"), json!(UUID)]),
            json!({"type": "string"})
        );
        assert_eq!(
            merged(&[json!(UUID), json!("a@example.com")]),
            json!({"type": "string"})
        );
        assert_eq!(schema(json!("")), json!({"type": "string"}));
    }

    #[test]
    fn dsc_03_required_is_present_in_every_object() {
        let s = merged(&[
            json!({"a": 1, "b": "x"}),
            json!({"a": 2}),
            json!({"a": 3, "c": null}),
        ]);
        assert_eq!(s["required"], json!(["a"]));
        assert_eq!(
            s["properties"],
            json!({"a": {"type": "integer"}, "b": {"type": "string"}, "c": {"type": "null"}})
        );
        assert_eq!(schema(json!({})), json!({"type": "object"}));
    }

    #[test]
    fn dsc_03_array_items_over_the_first_256() {
        let mut items: Vec<Value> = (0..256).map(|i| json!(i)).collect();
        items.push(json!("late"));
        assert_eq!(
            schema(Value::Array(items)),
            json!({"type": "array", "items": {"type": "integer"}})
        );
        let mut items: Vec<Value> = (0..255).map(|i| json!(i)).collect();
        items.push(json!("last"));
        assert_eq!(
            schema(Value::Array(items))["items"]["type"],
            json!(["integer", "string"])
        );
        assert_eq!(schema(json!([])), json!({"type": "array"}));
    }

    #[test]
    fn dsc_03_object_with_129_keys_is_a_map() {
        let obj = |n: usize| -> Value {
            Value::Object((0..n).map(|i| (format!("k{i:03}"), json!(i))).collect())
        };
        let s = schema(obj(128));
        assert_eq!(s["properties"].as_object().unwrap().len(), 128);
        assert!(s.get("additionalProperties").is_none());

        let s = schema(obj(129));
        assert_eq!(
            s,
            json!({"type": "object", "additionalProperties": {"type": "integer"}})
        );
    }

    #[test]
    fn dsc_03_map_after_merge_when_the_union_exceeds_128() {
        let half = |from: usize, v: Value| -> Value {
            Value::Object(
                (from..from + 70)
                    .map(|i| (format!("k{i:03}"), v.clone()))
                    .collect(),
            )
        };
        let a = half(0, json!(1));
        let b = half(60, json!("x"));
        let one = Schema::infer(&a).merge(Schema::infer(&b)).to_json();
        let other = Schema::infer(&b).merge(Schema::infer(&a)).to_json();
        assert_eq!(one, other);
        assert_eq!(
            one["additionalProperties"]["type"],
            json!(["integer", "string"])
        );
        assert!(one.get("properties").is_none());
    }

    #[test]
    fn dsc_03_level_17_is_empty() {
        // Object at level 1..=16, the value under the innermost one is level 17.
        let mut s = schema(nested(16));
        for _ in 0..15 {
            s = s["properties"]["a"].clone();
        }
        assert_eq!(s["properties"]["a"], json!({}));

        let mut s = schema(nested(15));
        for _ in 0..14 {
            s = s["properties"]["a"].clone();
        }
        assert_eq!(s["properties"]["a"], json!({"type": "integer"}).clone());
    }
}

mod params {
    use super::*;

    #[test]
    fn dsc_03_query_keys_are_percent_decoded() {
        let q = query(&["a%20b=1&caf%C3%A9=x&p+q=2"]);
        let props = q["properties"].as_object().unwrap();
        assert!(props.contains_key("a b"));
        assert!(props.contains_key("café"));
        assert!(props.contains_key("p q"));
    }

    #[test]
    fn dsc_03_repeated_key_is_an_array() {
        let q = query(&["tag=a&tag=b&one=1"]);
        assert_eq!(
            q["properties"]["tag"],
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(q["properties"]["one"], json!({"type": "string"}));
    }

    #[test]
    fn dsc_03_key_missing_once_is_not_required() {
        let q = query(&["a=1&b=2", "a=3", "a=4&b=5"]);
        assert_eq!(q["required"], json!(["a"]));
        let q = query(&["a=1&b=2", "b=3&a=4"]);
        assert_eq!(q["required"], json!(["a", "b"]));
        // A key with no value is present.
        assert_eq!(query(&["flag"])["required"], json!(["flag"]));
    }

    #[test]
    fn dsc_03_query_values_get_formats() {
        let q = query(&[&format!("id={UUID}")]);
        assert_eq!(
            q["properties"]["id"],
            json!({"type": "string", "format": "uuid"})
        );
    }

    #[test]
    fn dsc_03_query_keeps_64_keys_first_in_byte_order() {
        let qs: String = (0..100).rev().map(|i| format!("k{i:03}=1&")).collect();
        let q = query(&[&qs]);
        let keys: Vec<&String> = q["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 64);
        assert_eq!(keys[0], "k000");
        assert_eq!(keys[63], "k063");
        assert_eq!(q["required"].as_array().unwrap().len(), 64);
    }

    #[test]
    fn dsc_03_query_cap_is_the_same_whatever_the_merge_order() {
        let a: String = (0..50).map(|i| format!("k{:03}=1&", i * 2)).collect();
        let b: String = (0..50).map(|i| format!("k{:03}=1&", i * 2 + 1)).collect();
        let c: String = (0..20).map(|i| format!("j{i:03}=1&")).collect();
        let p = |s: &str| Params::from_query(&redactor(), s);
        let left = p(&a).merge(p(&b)).merge(p(&c));
        let right = p(&a).merge(p(&b).merge(p(&c)));
        let swapped = p(&c).merge(p(&b)).merge(p(&a));
        assert_eq!(left, right);
        assert_eq!(left, swapped);
        assert_eq!(left.to_json()["properties"].as_object().unwrap().len(), 64);
    }

    #[test]
    fn dsc_03_header_names_lower_case_and_skipped_names_absent() {
        let headers = [
            ("X-Request-Id", UUID),
            ("Accept", "*/*"),
            ("Connection", "close"),
            ("Keep-Alive", "5"),
            ("Transfer-Encoding", "chunked"),
            ("TE", "trailers"),
            ("Upgrade", "websocket"),
            ("Content-Length", "3"),
            ("Host", "api.local"),
            (":authority", "api.local"),
            (":path", "/"),
        ];
        let h = Params::from_headers(&redactor(), &headers).to_json();
        let keys: Vec<&String> = h["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["accept", "x-request-id"]);
        assert_eq!(
            h["properties"]["x-request-id"],
            json!({"type": "string", "format": "uuid"})
        );
    }

    #[test]
    fn dsc_03_headers_keep_64_names() {
        let names: Vec<String> = (0..100).map(|i| format!("x-h{i:03}")).collect();
        let headers: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "v")).collect();
        let h = Params::from_headers(&redactor(), &headers).to_json();
        assert_eq!(h["properties"].as_object().unwrap().len(), 64);
        assert!(h["properties"].get("x-h063").is_some());
        assert!(h["properties"].get("x-h064").is_none());
    }
}

mod bodies {
    use super::*;

    fn body_json(o: &Observed, side: &str) -> Value {
        o.to_json()[side]["body"].clone()
    }

    #[test]
    fn dsc_03_json_body_gets_a_schema_per_media_type() {
        let body = br#"{"id": 1, "name": "x"}"#;
        let obs = observed(&Observation {
            req_headers: &[("Content-Type", "application/json; charset=utf-8")],
            req_body: Some(body),
            ..Default::default()
        });
        let b = body_json(&obs, "request");
        assert_eq!(b["application/json"]["count"], json!(1));
        assert_eq!(b["application/json"]["bytes"], json!(body.len()));
        assert_eq!(
            b["application/json"]["schema"]["required"],
            json!(["id", "name"])
        );
    }

    #[test]
    fn dsc_03_plus_json_is_json() {
        let obs = observed(&Observation {
            resp_headers: &[("content-type", "application/problem+json")],
            resp_body: Some(br#"{"title": "x"}"#),
            ..Default::default()
        });
        let b = body_json(&obs, "response");
        assert_eq!(
            b["application/problem+json"]["schema"]["properties"]["title"],
            json!({"type": "string"})
        );
    }

    #[test]
    fn dsc_03_form_body_equals_the_equivalent_query() {
        let obs = observed(&Observation {
            query: "a=1&b=2&b=3",
            req_headers: &[("content-type", "application/x-www-form-urlencoded")],
            req_body: Some(b"a=1&b=2&b=3"),
            ..Default::default()
        });
        let j = obs.to_json();
        assert_eq!(
            j["request"]["body"]["application/x-www-form-urlencoded"]["schema"],
            j["query"]
        );
    }

    #[test]
    fn dsc_03_opaque_bodies_keep_media_type_and_size_only() {
        for media in ["text/plain", "application/grpc", "application/octet-stream"] {
            let obs = observed(&Observation {
                req_headers: &[("content-type", media)],
                req_body: Some(b"hello world"),
                ..Default::default()
            });
            let b = body_json(&obs, "request");
            assert_eq!(b[media], json!({"count": 1, "bytes": 11}), "{media}");
            assert!(!serde_json::to_string(&obs).unwrap().contains("hello"));
        }
    }

    #[test]
    fn dsc_03_unparsable_or_oversized_json_is_opaque() {
        let obs = observed(&Observation {
            req_headers: &[("content-type", "application/json")],
            req_body: Some(b"{not json"),
            ..Default::default()
        });
        assert_eq!(
            body_json(&obs, "request")["application/json"],
            json!({"count": 1, "bytes": 9})
        );
        let big = format!("[{}]", "1,".repeat(200 * 1024) + "1");
        assert!(big.len() > 256 * 1024);
        let obs = observed(&Observation {
            req_headers: &[("content-type", "application/json")],
            req_body: Some(big.as_bytes()),
            ..Default::default()
        });
        assert_eq!(
            body_json(&obs, "request")["application/json"],
            json!({"count": 1, "bytes": big.len()})
        );
    }

    #[test]
    fn dsc_03_form_body_over_the_cap_is_opaque() {
        let big = "a=1&".repeat(70 * 1024);
        let obs = observed(&Observation {
            req_headers: &[("content-type", "application/x-www-form-urlencoded")],
            req_body: Some(big.as_bytes()),
            ..Default::default()
        });
        assert_eq!(
            body_json(&obs, "request")["application/x-www-form-urlencoded"],
            json!({"count": 1, "bytes": big.len()})
        );
    }

    #[test]
    fn dsc_03_bodies_without_content_type_are_ignored_and_counts_add() {
        let obs = observed(&Observation {
            req_body: Some(b"{}"),
            ..Default::default()
        });
        assert_eq!(body_json(&obs, "request"), json!({}));
        let one = Observation {
            req_headers: &[("content-type", "application/json")],
            req_body: Some(b"{\"a\":1}"),
            ..Default::default()
        };
        let mut obs = Observed::default();
        obs.record(&redactor(), &one);
        obs.record(&redactor(), &one);
        let b = body_json(&obs, "request");
        assert_eq!(b["application/json"]["count"], json!(2));
        assert_eq!(b["application/json"]["bytes"], json!(14));
    }
}

mod volatility {
    use super::*;

    fn observe(values: impl IntoIterator<Item = String>) -> Volatility {
        let mut v = Volatility::default();
        for s in values {
            v.observe("f", &s, false);
        }
        v
    }

    #[test]
    fn dsc_04_three_distinct_values_are_volatile_two_are_not() {
        assert!(observe(["a", "b", "c"].map(String::from)).is_volatile("f"));
        assert!(!observe(["a", "b"].map(String::from)).is_volatile("f"));
        assert!(!observe(["a", "a", "a"].map(String::from)).is_volatile("f"));
    }

    #[test]
    fn dsc_04_ten_observations_of_two_values_are_not_volatile() {
        assert!(!observe((0..10).map(|i| (i % 2).to_string())).is_volatile("f"));
    }

    #[test]
    fn dsc_04_a_thousand_distinct_are_volatile_a_hundred_values_are_not() {
        assert!(observe((0..1000).map(|i| format!("id-{i}"))).is_volatile("f"));
        assert!(!observe((0..1000).map(|i| format!("v-{}", i % 100))).is_volatile("f"));
    }

    #[test]
    fn dsc_04_sketch_holds_at_most_32_hashes_and_counts_exactly_below() {
        let mut s = Sketch::default();
        for i in 0..20 {
            s.insert(&format!("v{i}"));
            s.insert(&format!("v{i}"));
        }
        assert_eq!((s.len(), s.distinct()), (20, 20));
        for i in 0..5000 {
            s.insert(&format!("w{i}"));
        }
        assert_eq!(s.len(), 32);
        let d = s.distinct();
        assert!((2500..10_000).contains(&d), "{d}");
    }

    #[test]
    fn dsc_04_merging_sketches_equals_the_sketch_of_the_union() {
        let of = |r: std::ops::Range<u32>| {
            let mut s = Sketch::default();
            for i in r {
                s.insert(&i.to_string());
            }
            s
        };
        assert_eq!(of(0..300).merge(of(200..700)), of(0..700));
        assert_eq!(of(0..5).merge(of(3..9)), of(0..9));
    }

    #[test]
    fn dsc_04_merged_observation_counts_add() {
        let a = observe(["a", "b"].map(String::from));
        let b = observe(["c"].map(String::from));
        assert!(!a.is_volatile("f"));
        assert!(!b.is_volatile("f"));
        assert!(a.merge(b).is_volatile("f"));
    }

    #[test]
    fn dsc_04_at_most_128_fields_first_in_byte_order() {
        let mut v = Volatility::default();
        for i in (0..200).rev() {
            v.observe(&format!("f{i:03}"), "x", false);
        }
        assert_eq!(v.len(), 128);
        let fields = v.fields();
        assert_eq!(fields[0], "f000");
        assert_eq!(fields[127], "f127");

        let mut a = Volatility::default();
        let mut b = Volatility::default();
        for i in 0..200 {
            if i % 2 == 0 { &mut a } else { &mut b }.observe(&format!("f{i:03}"), "x", false);
        }
        assert_eq!(a.clone().merge(b.clone()), v);
        assert_eq!(b.merge(a), v);
    }

    #[test]
    fn dsc_04_redacted_field_is_marked() {
        let mut v = Volatility::default();
        v.observe("secret", "<redacted>", true);
        v.observe("plain", "x", false);
        assert!(v.is_redacted("secret"));
        assert!(!v.is_redacted("plain"));
        // The mark survives a merge with an unmarked side.
        let mut w = Volatility::default();
        w.observe("secret", "x", false);
        assert!(v.merge(w).is_redacted("secret"));
    }

    fn fields(o: &Observed) -> Value {
        o.to_json()["fields"].clone()
    }

    #[test]
    fn dsc_04_fields_come_from_query_headers_and_json_leaves() {
        let mut obs = Observed::default();
        for i in 0..5 {
            let q = format!("page=1&key={i}");
            let body =
                json!({"items": [{"id": i * 10}, {"id": i * 10 + 1}], "kind": "x"}).to_string();
            let key = i.to_string();
            obs.record(
                &redactor(),
                &Observation {
                    query: &q,
                    req_headers: &[("content-type", "application/json"), ("x-nonce", &key)],
                    req_body: Some(body.as_bytes()),
                    ..Default::default()
                },
            );
        }
        let f = fields(&obs);
        assert_eq!(f["query/key"]["volatile"], json!(true));
        assert_eq!(f["query/page"]["volatile"], json!(false));
        assert_eq!(f["request.header/x-nonce"]["volatile"], json!(true));
        assert_eq!(
            f["request.body/application/json/items/*/id"]["volatile"],
            json!(true)
        );
        assert_eq!(
            f["request.body/application/json/items/*/id"]["n"],
            json!(10)
        );
        assert_eq!(
            f["request.body/application/json/kind"]["volatile"],
            json!(false)
        );
    }

    #[test]
    fn dsc_04_redacted_leaves_are_marked_and_hold_no_value() {
        let mut obs = Observed::default();
        for i in 0..4 {
            let q = format!("api_key=sk-secret-{i}&ok=1");
            let body = json!({"password": format!("hunter{i}"), "n": 1}).to_string();
            let auth = format!("Bearer tok-{i}");
            obs.record(
                &redactor(),
                &Observation {
                    query: &q,
                    req_headers: &[
                        ("content-type", "application/json"),
                        ("authorization", &auth),
                    ],
                    req_body: Some(body.as_bytes()),
                    ..Default::default()
                },
            );
        }
        let f = fields(&obs);
        assert_eq!(f["query/api_key"]["redacted"], json!(true));
        assert_eq!(f["request.header/authorization"]["redacted"], json!(true));
        assert_eq!(
            f["request.body/application/json/password"]["redacted"],
            json!(true)
        );
        assert_eq!(f["query/ok"]["redacted"], json!(false));
    }
}

mod no_leak {
    use super::*;

    #[test]
    fn dsc_03_no_redacted_value_is_kept() {
        let body = json!({
            "password": "hunter2-very-secret",
            "card": "4111 1111 1111 1111",
            "nested": {"api_key": {"deep": "deep-secret-value"}},
            "jwt": JWT,
            "user": "plain-user-name",
            "mail": "a@example.com",
        })
        .to_string();
        let form = "token=form-secret-value&x=1";
        let headers = [
            ("content-type", "application/json"),
            ("authorization", "Bearer bearer-secret-value"),
            ("cookie", "sid=cookie-secret-value"),
            ("x-jwt", JWT),
        ];
        let mut obs = observed(&Observation {
            query: "access_token=query-secret-value&ghp=ghp_abcdefghijklmnop",
            req_headers: &headers,
            req_body: Some(body.as_bytes()),
            resp_headers: &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("set-cookie", "sid=setcookie-secret-value; HttpOnly"),
            ],
            resp_body: Some(form.as_bytes()),
        });
        obs = obs.clone().merge(obs);
        for wire in [
            serde_json::to_string(&obs).unwrap(),
            obs.to_json().to_string(),
        ] {
            for secret in [
                "hunter2",
                "4111",
                "deep-secret",
                "bearer-secret",
                "cookie-secret",
                "setcookie-secret",
                "query-secret",
                "form-secret",
                "ghp_abc",
                "eyJhbGci",
                "c2ln",
            ] {
                assert!(!wire.contains(secret), "{secret} in {wire}");
            }
        }
        // The structure around the secrets is still described.
        let j = obs.to_json();
        let schema = &j["request"]["body"]["application/json"]["schema"];
        assert_eq!(schema["properties"]["password"], json!({"type": "string"}));
        assert_eq!(
            schema["properties"]["nested"]["properties"]["api_key"],
            json!({"type": "string"})
        );
        assert_eq!(schema["properties"]["mail"]["format"], json!("email"));
    }

    #[test]
    fn dsc_03_extra_redaction_names_apply_to_schemas() {
        let r = Redactor::new(["x-tenant"]);
        let p = Params::from_headers(&r, &[("X-Tenant", "acme-corp")]);
        assert!(!serde_json::to_string(&p).unwrap().contains("acme"));
    }
}

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!(null)),
        any::<bool>().prop_map(Value::from),
        (-3i64..3).prop_map(Value::from),
        prop_oneof![Just(0.5f64), Just(2.0), Just(-1.25)].prop_map(Value::from),
        prop_oneof![
            Just(UUID.to_string()),
            Just("a@example.com".to_string()),
            Just("2024-05-06".to_string()),
            Just("10.0.0.1".to_string()),
            Just("plain".to_string()),
            Just(String::new()),
        ]
        .prop_map(Value::from),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(5, 48, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[a-d]", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

#[derive(Debug, Clone)]
struct Owned {
    query: String,
    req_headers: Vec<(String, String)>,
    req_media: usize,
    req_body: Option<Vec<u8>>,
    resp_headers: Vec<(String, String)>,
    resp_body: Option<Vec<u8>>,
}

const MEDIA: [&str; 4] = [
    "application/json",
    "application/x-www-form-urlencoded",
    "text/plain",
    "application/problem+json",
];

fn body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        json_value().prop_map(|v| v.to_string().into_bytes()),
        "[a-c=&%0-9]{0,12}".prop_map(String::into_bytes),
    ]
}

fn owned() -> impl Strategy<Value = Owned> {
    let header = (
        "[a-cA-C-]{1,2}",
        prop_oneof![Just(UUID.to_string()), "[a-z0-9]{0,4}"],
    );
    (
        "([a-c]{1,2}(=[a-z0-9]{0,3})?&){0,4}",
        prop::collection::vec(header.clone(), 0..4),
        0usize..4,
        prop::option::of(body()),
        prop::collection::vec(header, 0..4),
        prop::option::of(body()),
    )
        .prop_map(
            |(query, req_headers, req_media, req_body, resp_headers, resp_body)| Owned {
                query,
                req_headers,
                req_media,
                req_body,
                resp_headers,
                resp_body,
            },
        )
}

fn with_observation<R>(o: &Owned, f: impl FnOnce(&Observation<'_>) -> R) -> R {
    let mut req: Vec<(&str, &str)> = o
        .req_headers
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    req.push(("content-type", MEDIA[o.req_media]));
    let mut resp: Vec<(&str, &str)> = o
        .resp_headers
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    resp.push(("content-type", MEDIA[(o.req_media + 1) % 4]));
    f(&Observation {
        query: &o.query,
        req_headers: &req,
        req_body: o.req_body.as_deref(),
        resp_headers: &resp,
        resp_body: o.resp_body.as_deref(),
    })
}

fn one(o: &Owned) -> Observed {
    let mut obs = Observed::default();
    with_observation(o, |ob| obs.record(&redactor(), ob));
    obs
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn dsc_09_schema_merge_is_commutative_and_associative(
        a in json_value(), b in json_value(), c in json_value()
    ) {
        let (a, b, c) = (Schema::infer(&a), Schema::infer(&b), Schema::infer(&c));
        prop_assert_eq!(a.clone().merge(b.clone()), b.clone().merge(a.clone()));
        prop_assert_eq!(
            a.clone().merge(b.clone()).merge(c.clone()),
            a.merge(b.merge(c))
        );
    }

    #[test]
    fn dsc_09_schema_json_round_trips(v in json_value()) {
        let s = Schema::infer(&v);
        let wire = serde_json::to_string(&s).unwrap();
        prop_assert_eq!(serde_json::from_str::<Schema>(&wire).unwrap(), s);
    }

    #[test]
    fn dsc_09_observed_merge_is_commutative_and_associative(
        a in owned(), b in owned(), c in owned()
    ) {
        let (a, b, c) = (one(&a), one(&b), one(&c));
        prop_assert_eq!(a.clone().merge(b.clone()), b.clone().merge(a.clone()));
        prop_assert_eq!(
            a.clone().merge(b.clone()).merge(c.clone()),
            a.merge(b.merge(c))
        );
    }

    #[test]
    fn dsc_09_recording_in_sequence_equals_merging_any_partition(
        seq in prop::collection::vec(owned(), 1..8),
        cuts in prop::collection::vec(any::<bool>(), 8)
    ) {
        let mut whole = Observed::default();
        for o in &seq {
            with_observation(o, |ob| whole.record(&redactor(), ob));
        }
        let mut parts = vec![Observed::default()];
        for (i, o) in seq.iter().enumerate() {
            if i > 0 && cuts[i] {
                parts.push(Observed::default());
            }
            let last = parts.last_mut().unwrap();
            with_observation(o, |ob| last.record(&redactor(), ob));
        }
        let merged = parts.into_iter().reduce(Observed::merge).unwrap();
        prop_assert_eq!(merged, whole);
    }

    #[test]
    fn dsc_09_observed_serialisation_is_stable_and_round_trips(a in owned(), b in owned()) {
        let x = one(&a).merge(one(&b));
        let y = one(&b).merge(one(&a));
        let (xs, ys) = (serde_json::to_string(&x).unwrap(), serde_json::to_string(&y).unwrap());
        prop_assert_eq!(&xs, &ys);
        prop_assert_eq!(serde_json::from_str::<Observed>(&xs).unwrap(), x.clone());
        prop_assert_eq!(x.to_json().to_string(), y.to_json().to_string());
    }

    #[test]
    fn dsc_09_sketches_merge_like_a_union(
        a in prop::collection::vec(0u16..400, 0..120),
        b in prop::collection::vec(0u16..400, 0..120)
    ) {
        let of = |xs: &[u16]| {
            let mut s = Sketch::default();
            xs.iter().for_each(|x| s.insert(&x.to_string()));
            s
        };
        let both: Vec<u16> = a.iter().chain(&b).copied().collect();
        prop_assert_eq!(of(&a).merge(of(&b)), of(&both));
        prop_assert_eq!(of(&a).merge(of(&b)), of(&b).merge(of(&a)));
    }
}

mod cap {
    use super::*;

    const CAP: usize = 512;

    /// Wide objects (some over 128 keys, so maps), long arrays and deep chains.
    fn heavy() -> impl Strategy<Value = Value> {
        scalar().prop_recursive(4, 400, 40, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..12).prop_map(Value::Array),
                prop::collection::btree_map("[a-f]{1,2}", inner.clone(), 0..40)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
                prop::collection::vec(("[a-z]{1,2}", inner), 100..200)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    fn chain(keys: &[String], leaf: Value) -> Value {
        keys.iter()
            .rev()
            .fold(leaf, |inner, k| json!({ k.as_str(): inner, "z": 1 }))
    }

    fn deep() -> impl Strategy<Value = Value> {
        (prop::collection::vec("[a-e]{1,2}", 1..22), heavy_small())
            .prop_map(|(keys, leaf)| chain(&keys, leaf))
    }

    fn heavy_small() -> impl Strategy<Value = Value> {
        scalar().prop_recursive(3, 30, 6, |inner| {
            prop::collection::btree_map("[a-d]", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect()))
        })
    }

    fn body_kind() -> impl Strategy<Value = Value> {
        prop_oneof![heavy(), deep(), json_value()]
    }

    #[test]
    fn dsc_11_ten_thousand_wide_bodies_keep_the_schema_under_512_nodes() {
        let mut acc: Option<Schema> = None;
        for i in 0..10_000u32 {
            let body: Value = (0..100)
                .map(|j| (format!("w{i}_{j}"), json!({ format!("f{}", i % 7): j })))
                .collect::<serde_json::Map<_, _>>()
                .into();
            let s = Schema::infer(&body);
            assert!(s.nodes() <= CAP, "body {i}: {}", s.nodes());
            let m = match acc.take() {
                Some(a) => a.merge(s),
                None => s,
            };
            assert!(m.nodes() <= CAP, "after {i}: {}", m.nodes());
            acc = Some(m);
        }
    }

    #[test]
    fn dsc_11_ten_thousand_deep_bodies_keep_the_schema_under_512_nodes() {
        let mut acc: Option<Schema> = None;
        for i in 0..10_000u32 {
            let keys: Vec<String> = (0..15).map(|d| format!("k{d}_{}", i >> (d % 12))).collect();
            let body = json!({ "items": [chain(&keys, json!(i))], format!("x{i}"): 1 });
            let s = Schema::infer(&body);
            assert!(s.nodes() <= CAP, "body {i}: {}", s.nodes());
            let m = match acc.take() {
                Some(a) => a.merge(s),
                None => s,
            };
            assert!(m.nodes() <= CAP, "after {i}: {}", m.nodes());
            acc = Some(m);
        }
    }

    #[test]
    fn dsc_11_cut_level_is_the_deepest_that_fits() {
        // A map of 300 objects of 3 nodes weighs 1 + 256 * 3: the values are cut to `{}`.
        let wide: Value = (0..300)
            .map(|j| (format!("k{j}"), json!({ "a": 1, "b": 2 })))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let s = Schema::infer(&wide);
        assert!(s.nodes() <= CAP);
        let j = s.to_json();
        assert_eq!(j["additionalProperties"], json!({}), "{j}");
        // 100 keys of 5 nodes each: 501 nodes fit whole.
        let fits: Value = (0..100)
            .map(|j| (format!("k{j}"), json!({ "a": 1, "b": { "c": 1, "d": 1 } })))
            .collect::<serde_json::Map<_, _>>()
            .into();
        assert_eq!(Schema::infer(&fits).nodes(), 501);
        let cut: Value = (0..100)
            .map(|j| {
                (
                    format!("k{j}"),
                    json!({ "a": 1, "b": { "c": 1, "d": 1, "e": 1 } }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
        // 601 nodes: the third level is cut, leaving 1 + 100 + 200 placeholders.
        let s = Schema::infer(&cut);
        assert_eq!(s.nodes(), 301);
        assert_eq!(
            s.to_json()["properties"]["k0"]["properties"]["b"],
            json!({})
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        #[test]
        fn dsc_11_capped_merge_is_commutative_and_associative(
            a in body_kind(), b in body_kind(), c in body_kind()
        ) {
            let (a, b, c) = (Schema::infer(&a), Schema::infer(&b), Schema::infer(&c));
            prop_assert!(a.nodes() <= CAP);
            let ab = a.clone().merge(b.clone());
            prop_assert!(ab.nodes() <= CAP);
            prop_assert_eq!(&ab, &b.clone().merge(a.clone()));
            let left = ab.merge(c.clone());
            prop_assert!(left.nodes() <= CAP);
            prop_assert_eq!(left, a.merge(b.merge(c)));
        }

        #[test]
        fn dsc_11_capped_schema_equals_any_partition(
            seq in prop::collection::vec(body_kind(), 1..8),
            cuts in prop::collection::vec(any::<bool>(), 8)
        ) {
            let whole = seq.iter().map(Schema::infer).reduce(Schema::merge).unwrap();
            let mut groups: Vec<Vec<&Value>> = vec![vec![]];
            for (i, v) in seq.iter().enumerate() {
                if i > 0 && cuts[i] {
                    groups.push(vec![]);
                }
                groups.last_mut().unwrap().push(v);
            }
            let parts = groups
                .into_iter()
                .map(|g| g.into_iter().map(Schema::infer).reduce(Schema::merge).unwrap())
                .reduce(Schema::merge)
                .unwrap();
            prop_assert!(whole.nodes() <= CAP);
            prop_assert_eq!(parts, whole);
        }

        #[test]
        fn dsc_11_capped_observed_equals_any_partition(
            seq in prop::collection::vec(body_kind(), 1..6),
            cuts in prop::collection::vec(any::<bool>(), 6)
        ) {
            let record = |vs: &[&Value]| {
                let mut o = Observed::default();
                for v in vs {
                    let body = v.to_string();
                    o.record(&redactor(), &Observation {
                        query: "",
                        req_headers: &[("content-type", "application/json")],
                        req_body: Some(body.as_bytes()),
                        resp_headers: &[],
                        resp_body: None,
                    });
                }
                o
            };
            let all: Vec<&Value> = seq.iter().collect();
            let whole = record(&all);
            let mut groups: Vec<Vec<&Value>> = vec![vec![]];
            for (i, v) in seq.iter().enumerate() {
                if i > 0 && cuts[i] {
                    groups.push(vec![]);
                }
                groups.last_mut().unwrap().push(v);
            }
            let parts = groups.iter().map(|g| record(g)).reduce(Observed::merge).unwrap();
            prop_assert_eq!(parts, whole);
        }
    }
}

mod names {
    use super::*;

    /// 127 ASCII bytes then a two-byte character: 129 bytes, the cut falls inside it.
    fn straddling() -> String {
        format!("{}é", "a".repeat(127))
    }

    #[test]
    fn dsc_11_long_names_are_cut_on_a_character_boundary() {
        let cut = "a".repeat(127);
        let ascii = "b".repeat(300);
        let ascii_cut = "b".repeat(128);

        let q = Params::from_query(&redactor(), &format!("{}=1&{ascii}=2", straddling())).to_json();
        let keys: Vec<&String> = q["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, [&cut, &ascii_cut]);

        let h = Params::from_headers(&redactor(), &[(ascii.as_str(), "x")]).to_json();
        assert!(h["properties"].get(&ascii_cut).is_some());

        let s = schema(json!({ straddling(): 1, ascii.clone(): {straddling(): 2} }));
        assert!(s["properties"].get(&cut).is_some());
        assert!(s["properties"][&ascii_cut]["properties"].get(&cut).is_some());
        assert_eq!(s["required"], json!([cut, ascii_cut]));
    }

    #[test]
    fn dsc_11_names_that_collide_after_the_cut_merge() {
        let a = format!("{}1", "k".repeat(128));
        let b = format!("{}2", "k".repeat(128));
        let s = schema(json!({ a: 1, b: "x" }));
        let props = s["properties"].as_object().unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(
            props["k".repeat(128).as_str()]["type"],
            json!(["integer", "string"])
        );
    }

    #[test]
    fn dsc_11_redaction_is_decided_on_the_full_name() {
        // The sensitive word starts at byte 126: it is not in the first 128 bytes.
        let name = format!("{}token", "a".repeat(126));
        let cut = &name[..128];
        let mut r = Redactor::default();
        let mut body = json!({ name.clone(): "plain-value" });
        r.json(&mut body);
        assert_eq!(body[&name], json!("<redacted>"));
        let _ = &mut r;

        let q = format!("{name}=plain-value");
        let hdr = [(name.as_str(), "plain-value")];
        let body = serde_json::to_vec(&json!({ name.clone(): "plain-value" })).unwrap();
        let ct = [("content-type", "application/json")];
        let o = Observation {
            query: &q,
            req_headers: &hdr,
            req_body: None,
            resp_headers: &ct,
            resp_body: Some(&body),
        };
        let f = observed(&o).to_json()["fields"].clone();
        for field in [
            format!("query/{cut}"),
            format!("request.header/{cut}"),
            format!("response.body/application/json/{cut}"),
        ] {
            assert_eq!(f[&field]["redacted"], json!(true), "{field}");
        }
        assert_eq!(f.as_object().unwrap().len(), 3);
    }

    #[test]
    fn dsc_11_state_stays_under_the_bound_with_longest_names() {
        let name = "é".repeat(5000);
        let body = serde_json::to_vec(&json!({ name.clone(): { name.clone(): 1 } })).unwrap();
        let q = format!("{name}=1");
        let hdr = [(name.as_str(), "x"), ("content-type", "application/json")];
        let o = Observation {
            query: &q,
            req_headers: &hdr,
            req_body: Some(&body),
            resp_headers: &hdr,
            resp_body: Some(&body),
        };
        let json = observed(&o).to_json().to_string();
        assert!(!json.contains(&"é".repeat(65)), "a name was not cut");
    }
}
