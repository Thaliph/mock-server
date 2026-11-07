//! PUL-01 to PUL-05: the egress set, its check, digest and ETag, and the local view
//! (spec §7.7). Canaries never appear in assertion messages.

use proptest::prelude::*;
use serde_json::{Value, json};
use xxhash_rust::xxh3::xxh3_64;

use super::egress::{EgressSet, FORMAT, Refusal, digest, etag, fresh_hex, matches_etag, summary};
use super::observed::Observation;
use super::openapi::document;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Report, Via, merge};
use super::scope::ScopeKey;
use super::tests_openapi::LIMITS;
use super::tests_push_v2::{exchange, record_all};
use super::tests_reredact_outbound::{form, hash_free, leak_all, plant, standard};

const AUTHORITY: &str = "api.example.com:443";

/// One exchange of `direction`, with an id that differs per call and a bearer token.
#[allow(clippy::too_many_arguments)]
fn record(
    r: &mut Report,
    direction: Direction,
    authority: &str,
    method: &'static str,
    path: &str,
    status: u16,
    n: usize,
    body: bool,
) {
    let id = format!("id-{n}");
    let headers = [
        ("authorization", "Bearer abc.def"),
        ("x-request-id", id.as_str()),
        ("content-type", "application/json"),
    ];
    let resp = [("content-type", "application/json")];
    let body_text = format!("{{\"id\":\"{id}\",\"ok\":true}}");
    let observation = Observation {
        query: "a=1",
        req_headers: &headers,
        req_body: body.then_some(body_text.as_bytes()),
        resp_headers: &resp,
        resp_body: body.then_some(body_text.as_bytes()),
    };
    let exchange = Exchange {
        direction,
        port: 8080,
        method,
        path,
        status,
        head_ms: 3 + n as u64,
        host: None,
        grpc: false,
        observation,
    };
    match direction {
        Direction::Inbound => r.record(&LIMITS, &Redactor::default(), &exchange),
        Direction::Outbound => r.record_at(
            ScopeKey::Authority(authority.into()),
            &Via {
                tls: true,
                sni: Some("api.example.com"),
                dst: Some("93.184.216.34:443".parse().expect("socket address")),
            },
            &LIMITS,
            &Redactor::default(),
            &exchange,
        ),
    };
}

/// A report with an inbound part, two authorities, a tunnel-only scope, volatile and redacted
/// fields, samples, and the controller's stamp.
fn mixed() -> Report {
    let mut r = Report::new(Some("pod-a".into()));
    for n in 0..5 {
        record(
            &mut r,
            Direction::Inbound,
            "",
            "GET",
            "/in/42",
            200,
            n,
            true,
        );
        let status = if n == 4 { 404 } else { 200 };
        record(
            &mut r,
            Direction::Outbound,
            AUTHORITY,
            "GET",
            "/v1/users/42",
            status,
            n,
            true,
        );
        record(
            &mut r,
            Direction::Outbound,
            AUTHORITY,
            "PUT",
            "/v1/users/42",
            200,
            n,
            true,
        );
        record(
            &mut r,
            Direction::Outbound,
            "10.0.0.5:8080",
            "POST",
            "/charge",
            201,
            n,
            n % 2 == 0,
        );
    }
    r.record_tunnel(
        &LIMITS,
        ScopeKey::Authority("pay.example:443".into()),
        "tcp",
    );
    r.stamp(20_000);
    r
}

fn project_json(r: &Report) -> String {
    EgressSet::project(r).to_json().expect("serialises")
}

/// Every key of every object, at any depth.
fn keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                out.push(k.clone());
                keys(v, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| keys(v, out)),
        _ => {}
    }
}

fn without_stamp(sample: &Value) -> Value {
    let mut sample = sample.clone();
    sample.as_object_mut().expect("object").remove("last_seen");
    sample
}

/// The `x-mimic-*` list of an operation of the §7.6 document, or empty.
fn names(op: &Value, key: &str) -> Value {
    op.get(key).cloned().unwrap_or_else(|| json!([]))
}

/// The projection holds what the report holds for each outbound endpoint, and the §7.6 lists.
fn assert_matches_report(r: &Report) -> Result<(), TestCaseError> {
    let set = EgressSet::project(r);
    prop_assert_eq!(set.format, FORMAT);
    let value: Value = serde_json::from_str(&set.to_json().expect("serialises")).expect("json");
    let expected: Vec<_> = r
        .endpoints()
        .into_iter()
        .filter(|e| e.direction == Direction::Outbound)
        .collect();
    let mut seen = 0;
    for (authority, entry) in value["authorities"].as_object().expect("object") {
        let doc = document(r, "ns", "dep", 1, &format!("outbound-{authority}")).expect("scope");
        let mine: Vec<_> = expected
            .iter()
            .filter(|e| e.authority == Some(authority.as_str()))
            .collect();
        let got = entry["endpoints"].as_array().expect("array");
        prop_assert!(!got.is_empty());
        prop_assert_eq!(got.len(), mine.len());
        for (g, e) in got.iter().zip(mine) {
            seen += 1;
            prop_assert_eq!(g["method"].as_str(), Some(e.method));
            prop_assert_eq!(g["template"].as_str(), Some(e.template.as_str()));
            prop_assert_eq!(g["grpc"].as_bool(), Some(e.grpc));
            prop_assert_eq!(
                &g["statuses"],
                &serde_json::to_value(&e.stats.statuses).unwrap()
            );
            prop_assert_eq!(
                &g["latency"],
                &serde_json::to_value(&e.stats.latency).unwrap()
            );
            let samples: Vec<Value> = e
                .stats
                .samples()
                .iter()
                .map(|(_, s)| without_stamp(&serde_json::to_value(s).unwrap()))
                .collect();
            prop_assert_eq!(g["samples"].as_array().expect("array"), &samples);
            let op = &doc["paths"][&e.template][e.method.to_lowercase()];
            prop_assert_eq!(&g["volatile"], &names(op, "x-mimic-volatile"));
            prop_assert_eq!(&g["redacted"], &names(op, "x-mimic-redacted"));
        }
    }
    prop_assert_eq!(seen, expected.len());
    let mut all = Vec::new();
    keys(&value, &mut all);
    for forbidden in [
        "schema",
        "origin",
        "tunnels",
        "hosts",
        "last_seen",
        "pod",
        "observed",
    ] {
        prop_assert!(!all.iter().any(|k| k == forbidden), "key {forbidden}");
    }
    prop_assert!(!all.iter().any(|k| k.contains("sketch")));
    Ok(())
}

#[test]
fn pul_01_projection_of_a_mixed_report() {
    let r = mixed();
    assert_matches_report(&r).expect("projection matches the report");
    let set = EgressSet::project(&r);
    let authorities: Vec<_> = set
        .authorities
        .keys()
        .map(compact_str::CompactString::as_str)
        .collect();
    // The tunnel-only scope and the inbound part are left out.
    assert_eq!(authorities, ["10.0.0.5:8080", AUTHORITY]);
    assert_eq!(set.endpoint_count(), 3);
    let api = &set.authorities[AUTHORITY].endpoints;
    assert_eq!(
        api.iter().map(|e| e.method.as_str()).collect::<Vec<_>>(),
        ["GET", "PUT"]
    );
    assert_eq!(api[0].template, "/v1/users/{users_id}");
    assert_eq!(api[0].statuses.len(), 2);
    assert!(!api[0].samples.is_empty());
    // The bearer is redacted, the request id changes at every call.
    assert!(
        api[0]
            .redacted
            .iter()
            .any(|f| f.ends_with("/authorization"))
    );
    assert!(
        !api[0].volatile.is_empty(),
        "the response id changes at every call"
    );
}

#[test]
fn pul_01_a_report_without_outbound_scope_is_the_empty_set() {
    let empty = r#"{"authorities":{},"format":1}"#;
    assert_eq!(project_json(&Report::new(None)), empty);
    let mut inbound = Report::new(None);
    record(
        &mut inbound,
        Direction::Inbound,
        "",
        "GET",
        "/a",
        200,
        0,
        true,
    );
    assert_eq!(project_json(&inbound), empty);
    let mut tunnels = Report::new(None);
    tunnels.record_tunnel(
        &LIMITS,
        ScopeKey::Authority("pay.example:443".into()),
        "tcp",
    );
    assert_eq!(project_json(&tunnels), empty);
    assert_eq!(
        EgressSet::from_json(empty)
            .expect("accepted")
            .endpoint_count(),
        0
    );
}

#[test]
fn pul_01_projection_is_byte_identical_and_reads_back() {
    let r = mixed();
    let json = project_json(&r);
    assert_eq!(json, project_json(&r.clone()));
    assert_eq!(
        json,
        project_json(&Report::from_json(&r.to_json().unwrap()).unwrap())
    );
    let back = EgressSet::from_json(&json).expect("a projection passes its check");
    assert_eq!(back.to_json().unwrap(), json);
    // Sorted keys at the top.
    assert!(json.starts_with(r#"{"authorities":{"#));
    assert!(json.ends_with(r#""format":1}"#));
}

#[test]
fn pul_01_the_projection_of_the_union_is_that_of_its_outbound_part() {
    let r = mixed();
    let (_, outbound) = r.split();
    let outbound = outbound.expect("an outbound part");
    assert_eq!(project_json(&r), project_json(&outbound));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn pul_01_projection_of_recorded_and_merged_reports(
        items in proptest::collection::vec(exchange(), 1..10),
        split in 0usize..11,
        stamped in any::<bool>(),
    ) {
        let split = split.min(items.len());
        let (a, b) = (record_all(&items[..split], 8), record_all(&items[split..], 8));
        let mut merged = merge(a, b, &LIMITS);
        if stamped {
            merged.stamp(20_000);
        }
        assert_matches_report(&merged)?;
        let json = project_json(&merged);
        prop_assert_eq!(&json, &project_json(&merged.clone()));
        let (_, outbound) = merged.split();
        let part = outbound.as_ref().map_or_else(|| project_json(&Report::new(None)), project_json);
        prop_assert_eq!(&json, &part);
        let back = EgressSet::from_json(&json);
        prop_assert!(back.is_ok(), "check");
        prop_assert_eq!(back.unwrap().to_json().unwrap(), json);
    }
}

// ---------------------------------------------------------------------------------------
// PUL-02: the check.

/// A valid set as JSON text and value: one authority, one endpoint, one sample.
fn base() -> Value {
    let mut r = Report::new(None);
    record(
        &mut r,
        Direction::Outbound,
        AUTHORITY,
        "GET",
        "/v1/ping",
        200,
        1,
        true,
    );
    serde_json::from_str(&project_json(&r)).expect("json")
}

fn endpoint(v: &mut Value) -> &mut Value {
    &mut v["authorities"][AUTHORITY]["endpoints"][0]
}

fn refusal(v: &Value) -> Result<EgressSet, Refusal> {
    EgressSet::from_json(&v.to_string())
}

fn assert_refused(v: &Value, want: Refusal, what: &str) {
    assert_eq!(refusal(v).err(), Some(want), "{what}");
}

#[test]
fn pul_02_the_base_is_accepted() {
    let v = base();
    assert!(refusal(&v).is_ok());
    assert_eq!(
        v["authorities"][AUTHORITY]["endpoints"][0]["samples"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn pul_02_format() {
    for format in [
        json!(0),
        json!(2),
        json!(4_294_967_296u64),
        json!("1"),
        json!(null),
    ] {
        let mut v = base();
        v["format"] = format.clone();
        assert_refused(&v, Refusal::Format, &format!("format {format}"));
    }
    let mut v = base();
    v.as_object_mut().unwrap().remove("format");
    assert_refused(&v, Refusal::Format, "no format");
    assert_eq!(
        EgressSet::from_json(r#"{"authorities":{}}"#).err(),
        Some(Refusal::Format)
    );
    assert_eq!(Refusal::Format.reason(), "format");
}

#[test]
fn pul_02_authority() {
    for key in [
        "A.example:80",
        "a.example:080",
        "a.example",
        "80",
        "a.example:0",
        "",
    ] {
        let mut v = base();
        let entry = v["authorities"][AUTHORITY].take();
        v["authorities"] = json!({ key: entry });
        assert_refused(&v, Refusal::Authority, key);
    }
    let mut v = base();
    let entry = v["authorities"][AUTHORITY].take();
    v["authorities"] = json!({ "[2001:db8::1]:8443": entry.clone(), "<redacted>:443": entry });
    assert!(
        refusal(&v).is_ok(),
        "an IPv6 literal and a caught host are authorities"
    );
    assert_eq!(Refusal::Authority.reason(), "authority");
}

/// An endpoint with nothing but the required fields.
fn bare(template: &str) -> Value {
    json!({
        "grpc": false, "latency": {"buckets": {"3": 1}}, "method": "GET",
        "redacted": [], "samples": [], "statuses": {"200": 1},
        "template": template, "volatile": []
    })
}

#[test]
fn pul_02_limits() {
    let set = |authorities: usize, per: usize| {
        let endpoints: Vec<Value> = (0..per).map(|i| bare(&format!("/e{i}"))).collect();
        let entries: serde_json::Map<String, Value> = (0..authorities)
            .map(|i| {
                (
                    format!("h{i}.example:80"),
                    json!({ "endpoints": endpoints }),
                )
            })
            .collect();
        json!({ "authorities": entries, "format": 1 })
    };
    assert!(refusal(&set(10_000, 1)).is_ok(), "10 000 authorities");
    assert_refused(&set(10_001, 1), Refusal::Limit, "10 001 authorities");
    assert!(refusal(&set(1, 10_000)).is_ok(), "10 000 endpoints");
    assert_refused(&set(1, 10_001), Refusal::Limit, "10 001 endpoints");
    assert_refused(&set(2, 5_001), Refusal::Limit, "10 002 endpoints in all");
    assert_eq!(Refusal::Limit.reason(), "limit");
}

#[test]
fn pul_02_a_method_and_template_twice_in_one_authority_is_malformed() {
    let mut v = base();
    let twice = endpoint(&mut v).clone();
    v["authorities"][AUTHORITY]["endpoints"]
        .as_array_mut()
        .unwrap()
        .push(twice);
    assert_refused(&v, Refusal::Malformed, "same method and template twice");
    // Another method, another template or another authority is not a duplicate.
    let mut other = base();
    let mut put = endpoint(&mut other).clone();
    put["method"] = "PUT".into();
    let mut path = endpoint(&mut other).clone();
    path["template"] = "/other".into();
    let endpoints = other["authorities"][AUTHORITY]["endpoints"]
        .as_array_mut()
        .unwrap();
    endpoints.push(put);
    endpoints.push(path);
    let entry = other["authorities"][AUTHORITY].clone();
    other["authorities"]["b.example.com:443"] = entry;
    assert!(refusal(&other).is_ok());
}

#[test]
fn pul_02_template() {
    let long = |n: usize| format!("/{}", "a".repeat(n - 1));
    let mut v = base();
    endpoint(&mut v)["template"] = long(2048).into();
    assert!(refusal(&v).is_ok(), "2048 bytes");
    endpoint(&mut v)["template"] = long(2049).into();
    assert_refused(&v, Refusal::Malformed, "2049 bytes");
    endpoint(&mut v)["template"] = "/a".repeat(32).into();
    assert!(refusal(&v).is_ok(), "32 segments");
    endpoint(&mut v)["template"] = "/a".repeat(33).into();
    assert_refused(&v, Refusal::Malformed, "33 segments");
    assert_eq!(Refusal::Malformed.reason(), "malformed");
}

#[test]
fn pul_02_statuses_and_histogram() {
    let mut v = base();
    for (status, ok) in [
        ("100", true),
        ("599", true),
        ("99", false),
        ("600", false),
        ("0", false),
    ] {
        endpoint(&mut v)["statuses"] = json!({ status: 1 });
        assert_eq!(refusal(&v).is_ok(), ok, "status {status}");
    }
    endpoint(&mut v)["statuses"] = json!({"200": 1});
    for (bucket, ok) in [("0", true), ("69", true), ("70", false), ("255", false)] {
        endpoint(&mut v)["latency"] = json!({"buckets": { bucket: 1 }});
        assert_eq!(refusal(&v).is_ok(), ok, "bucket {bucket}");
    }
    endpoint(&mut v)["latency"] = json!({"buckets": {"3": 1}});
    assert!(refusal(&v).is_ok());
}

#[test]
fn pul_02_samples() {
    let mut v = base();
    let sample = endpoint(&mut v)["samples"][0].clone();
    endpoint(&mut v)["samples"] = Value::Array(vec![sample.clone(); 20]);
    assert!(refusal(&v).is_ok(), "20 samples");
    endpoint(&mut v)["samples"] = Value::Array(vec![sample.clone(); 21]);
    assert_refused(&v, Refusal::Malformed, "21 samples");
    let mut bad = sample.clone();
    bad["bytes"] = json!(sample["bytes"].as_u64().unwrap() + 1);
    endpoint(&mut v)["samples"] = json!([bad]);
    assert_refused(&v, Refusal::Malformed, "bytes is not the size");
    let mut bad = sample.clone();
    bad["response"]["status"] = json!(99);
    endpoint(&mut v)["samples"] = json!([bad]);
    assert_refused(&v, Refusal::Malformed, "sample status");
    // A stamp is the controller's, never in a projection.
    let mut stamped = sample;
    stamped["last_seen"] = json!(20_000);
    endpoint(&mut v)["samples"] = json!([stamped]);
    assert_refused(&v, Refusal::Malformed, "a stamp");
}

#[test]
fn pul_02_field_names() {
    let name = |n: usize, c: char| c.to_string().repeat(n);
    let list = |from: usize, n: usize| -> Vec<String> {
        (from..from + n).map(|i| format!("f{i:03}")).collect()
    };
    let mut v = base();
    endpoint(&mut v)["redacted"] = json!([]);
    endpoint(&mut v)["volatile"] = json!(list(0, 128));
    assert!(refusal(&v).is_ok(), "128 names");
    endpoint(&mut v)["volatile"] = json!(list(0, 129));
    assert_refused(&v, Refusal::Malformed, "129 volatile names");
    endpoint(&mut v)["volatile"] = json!(list(0, 64));
    endpoint(&mut v)["redacted"] = json!(list(64, 65));
    assert_refused(&v, Refusal::Malformed, "129 names together");
    endpoint(&mut v)["redacted"] = json!(list(64, 64));
    assert!(refusal(&v).is_ok(), "128 names together");
    endpoint(&mut v)["volatile"] = json!([name(128, 'a')]);
    endpoint(&mut v)["redacted"] = json!([]);
    assert!(refusal(&v).is_ok(), "128 bytes");
    endpoint(&mut v)["volatile"] = json!([name(129, 'a')]);
    assert_refused(&v, Refusal::Malformed, "129 bytes");
    endpoint(&mut v)["volatile"] = json!([]);
    endpoint(&mut v)["redacted"] = json!([name(129, 'b')]);
    assert_refused(&v, Refusal::Malformed, "129 bytes, redacted");
}

#[test]
fn pul_02_unknown_fields_at_any_level_and_bodies_that_are_not_sets() {
    let places: [&dyn Fn(&mut Value); 6] = [
        &|v| v["extra"] = json!(1),
        &|v| v["authorities"][AUTHORITY]["extra"] = json!(1),
        &|v| endpoint(v)["extra"] = json!(1),
        &|v| endpoint(v)["latency"]["extra"] = json!(1),
        &|v| endpoint(v)["samples"][0]["extra"] = json!(1),
        &|v| endpoint(v)["samples"][0]["request"]["extra"] = json!(1),
    ];
    for (i, place) in places.iter().enumerate() {
        let mut v = base();
        place(&mut v);
        assert_refused(&v, Refusal::Malformed, &format!("unknown field {i}"));
    }
    for body in [
        "",
        "not json",
        "[]",
        "null",
        "{",
        "{\"format\":1",
        "\u{feff}{}",
        "{\"format\":1,\"authorities\":[]}",
    ] {
        assert_eq!(
            EgressSet::from_json(body).err(),
            Some(Refusal::Malformed),
            "{body:?}"
        );
    }
    // A missing endpoint field is malformed too.
    let mut v = base();
    endpoint(&mut v).as_object_mut().unwrap().remove("samples");
    assert_refused(&v, Refusal::Malformed, "no samples");
}

// ---------------------------------------------------------------------------------------
// PUL-03: no leak.

fn sketch_hashes(v: &Value, out: &mut Vec<u64>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                if k == "sketch" {
                    out.extend(
                        v["hashes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_u64),
                    );
                }
                sketch_hashes(v, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| sketch_hashes(v, out)),
        _ => {}
    }
}

/// The leaked outbound scope, copied under an inbound port as well.
fn with_inbound(text: &str) -> String {
    let mut v: Value = serde_json::from_str(text).expect("json");
    let mut scope = v["scopes"]["outbound"]
        .as_object()
        .and_then(|o| o.values().next().cloned())
        .expect("a scope");
    scope.as_object_mut().unwrap().remove("origin");
    v["scopes"]["inbound"] = json!({ "8080": scope });
    v.to_string()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn pul_03_no_canary_and_no_sketch_hash_in_the_projection(
        canary in "[a-z0-9]{35}",
        ua in 0usize..4,
        etag_form in 0usize..4,
        host_form in 0usize..2,
        both in any::<bool>(),
    ) {
        let prefix = ["sk-", "ghp_"];
        let host = format!("{}{canary}.example.com:443", prefix[host_form]);
        let mut text = plant(&leak_all(&standard(), &host, &[]), &canary, ua, etag_form);
        if both {
            text = with_inbound(&text);
        }
        let source: Value = serde_json::from_str(&text).expect("json");
        let mut r = Report::from_json(&text).expect("a report");
        r.re_redact(&Redactor::default());
        let mut hashes = Vec::new();
        sketch_hashes(&source, &mut hashes);
        sketch_hashes(&serde_json::from_str(&r.to_json().unwrap()).unwrap(), &mut hashes);
        let set = EgressSet::project(&r);
        let json = set.to_json().unwrap();
        let view = summary(Some((&set, "\"1-ab\"", "2026-01-01T00:00:00Z")));
        let needles = [canary.clone(), form(ua, &canary).1,
            form(etag_form, &canary).1];
        for needle in &needles {
            prop_assert!(hash_free(&json, needle));
            prop_assert!(hash_free(&view, needle));
        }
        for hash in hashes {
            for text in [json.as_str(), view.as_str()] {
                prop_assert!(!text.contains(&hash.to_string()));
                let hex = format!("{hash:x}");
                prop_assert!(!text.contains(&hex));
            }
        }
        // The canary hash of a source word that is not in the report is not a needle either.
        prop_assert!(!json.contains(&xxh3_64(canary.as_bytes()).to_string()));
        prop_assert!(EgressSet::from_json(&json).is_ok());
    }
}

// ---------------------------------------------------------------------------------------
// PUL-04: digest and ETag.

const HEX_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const HEX_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn pul_04_digest() {
    assert_eq!(digest(7, b"abc"), format!("7:{HEX_ABC}"));
    assert_eq!(digest(0, b""), format!("0:{HEX_EMPTY}"));
    assert_eq!(digest(123, b""), format!("123:{HEX_EMPTY}"));
    assert_ne!(digest(7, b"abc"), digest(7, b"abd"));
}

#[test]
fn pul_04_etag() {
    assert_eq!(etag("ab12"), "\"1-ab12\"");
    assert_eq!(etag(HEX_ABC), format!("\"1-{HEX_ABC}\""));
}

#[test]
fn pul_04_a_digest_that_is_absent_malformed_or_for_another_version_is_stale() {
    let ok = format!("7:{HEX_ABC}");
    assert_eq!(fresh_hex(Some(&ok), 7), Some(HEX_ABC));
    assert_eq!(fresh_hex(Some(&ok), 8), None, "another version");
    assert_eq!(fresh_hex(Some(&ok), 6), None, "another version");
    assert_eq!(fresh_hex(None, 7), None, "absent");
    let upper = format!("7:{}", HEX_ABC.to_uppercase());
    let bad = [
        String::new(),
        "7".to_string(),
        "7:".to_string(),
        HEX_ABC.to_string(),
        format!("07:{HEX_ABC}"),
        format!("+7:{HEX_ABC}"),
        format!(":{HEX_ABC}"),
        format!("7:{}", &HEX_ABC[1..]),
        format!("7:{HEX_ABC}0"),
        format!("7:{}g", &HEX_ABC[1..]),
        format!("7:{HEX_ABC} "),
        format!("7:{HEX_ABC}:x"),
        upper,
    ];
    for digest in &bad {
        assert_eq!(fresh_hex(Some(digest), 7), None, "{digest:?}");
    }
    // What `digest` writes is fresh.
    let written = digest(41, b"x");
    assert_eq!(
        fresh_hex(Some(&written), 41),
        Some(written.split_once(':').unwrap().1)
    );
}

#[test]
fn pul_04_if_none_match_matches_one_entry_exactly() {
    let tag = etag("ab");
    assert!(matches_etag(&tag, &tag));
    assert!(matches_etag("\"1-aa\", \"1-ab\"", &tag));
    assert!(matches_etag("\"1-aa\",\"1-ab\",\"1-ac\"", &tag));
    assert!(matches_etag("\t\"1-ab\" ", &tag));
    assert!(!matches_etag("W/\"1-ab\"", &tag), "weak");
    assert!(!matches_etag("*", &tag), "star");
    assert!(!matches_etag("\"1-abc\"", &tag));
    assert!(!matches_etag("\"1-a\"", &tag));
    assert!(!matches_etag("1-ab", &tag), "unquoted");
    assert!(!matches_etag("", &tag));
    assert!(!matches_etag(",", &tag));
    assert!(!matches_etag("\"1-AB\"", &tag));
    assert!(!matches_etag("\"1-ab\" \"1-ac\"", &tag), "no comma");
}

// ---------------------------------------------------------------------------------------
// PUL-05: the local view.

#[test]
fn pul_05_without_a_snapshot() {
    assert_eq!(
        summary(None),
        r#"{"authorities":{},"etag":null,"pulled":null}"#
    );
}

#[test]
fn pul_05_summary_of_a_snapshot() {
    let set = EgressSet::project(&mixed());
    let text = summary(Some((&set, "\"1-ab\"", "2026-11-06T10:00:00Z")));
    assert_eq!(
        text,
        summary(Some((&set.clone(), "\"1-ab\"", "2026-11-06T10:00:00Z")))
    );
    let v: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(v["etag"], "\"1-ab\"");
    assert_eq!(v["pulled"], "2026-11-06T10:00:00Z");
    let api = &v["authorities"][AUTHORITY]["endpoints"];
    assert_eq!(api.as_array().map(Vec::len), Some(2));
    assert_eq!(api[0]["method"], "GET");
    assert_eq!(api[0]["template"], "/v1/users/{users_id}");
    assert_eq!(api[0]["grpc"], false);
    assert_eq!(api[0]["statuses"], json!({"200": 4, "404": 1}));
    assert_eq!(api[0]["exchanges"], 5);
    assert_eq!(api[1]["method"], "PUT");
    assert!(v["authorities"]["10.0.0.5:8080"].is_object());
    // Keys sorted at every level, and nothing but the five fields per endpoint.
    assert!(text.starts_with(r#"{"authorities":{"#));
    assert!(text.contains(r#""etag":"\"1-ab\"","pulled":"#));
    for e in api.as_array().unwrap() {
        let mut ks: Vec<_> = e.as_object().unwrap().keys().cloned().collect();
        ks.sort();
        assert_eq!(ks, ["exchanges", "grpc", "method", "statuses", "template"]);
    }
    let mut all = Vec::new();
    keys(&v, &mut all);
    for forbidden in [
        "samples", "volatile", "redacted", "latency", "request", "response",
    ] {
        assert!(!all.iter().any(|k| k == forbidden), "key {forbidden}");
    }
    // The sample bodies and the field names are not in the view.
    assert!(!text.contains("x-request-id") && !text.contains("authorization"));
    assert!(!text.contains("id-1"));
    let key_order: Vec<usize> = ["\"authorities\"", "\"etag\"", "\"pulled\""]
        .iter()
        .map(|k| text.find(k).expect("key"))
        .collect();
    assert!(key_order.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn pul_05_an_empty_snapshot_still_has_an_etag() {
    let set = EgressSet::project(&Report::new(None));
    assert_eq!(
        summary(Some((&set, "\"1-ab\"", "2026-11-06T10:00:00Z"))),
        r#"{"authorities":{},"etag":"\"1-ab\"","pulled":"2026-11-06T10:00:00Z"}"#
    );
}
