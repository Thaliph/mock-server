//! CTL-03 and CTL-04: re-redaction and retention of the controller store (spec §7.3).

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};
use xxhash_rust::xxh3::xxh3_64;

use super::observed::Observation;
use super::redact::{MARKER, Redactor};
use super::report::{Direction, Exchange, Limits, Report, merge};
use super::store::{Kind, drop_expired, merge_bounded};

const LIMITS: Limits = Limits {
    max_literals: 32,
    max_endpoints: 256,
    sample_budget: 1 << 20,
};
const NAME: &str = "xtrafield";

type Headers = Vec<(String, String)>;

/// One `POST /p`, owned.
#[derive(Debug, Clone, Default)]
struct Input {
    query: String,
    req_headers: Headers,
    req_body: Option<String>,
    resp_headers: Headers,
    resp_body: Option<String>,
    host: Option<String>,
}

fn header(name: &str, value: &str) -> Headers {
    vec![(name.to_string(), value.to_string())]
}

/// Records `times` exchanges of `input` with `redactor`.
fn record(redactor: &Redactor, input: &Input, times: usize) -> Report {
    let req: Vec<(&str, &str)> = input
        .req_headers
        .iter()
        .map(|(n, v)| (&**n, &**v))
        .collect();
    let resp: Vec<(&str, &str)> = input
        .resp_headers
        .iter()
        .map(|(n, v)| (&**n, &**v))
        .collect();
    let mut report = Report::new(Some("pod".into()));
    for _ in 0..times {
        report.record(
            &LIMITS,
            redactor,
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "POST",
                path: "/p",
                status: 200,
                head_ms: 1,
                host: input.host.as_deref().or(Some("svc")),
                grpc: false,
                observation: Observation {
                    query: &input.query,
                    req_headers: &req,
                    req_body: input.req_body.as_deref().map(str::as_bytes),
                    resp_headers: &resp,
                    resp_body: input.resp_body.as_deref().map(str::as_bytes),
                },
            },
        );
    }
    report
}

/// Where a canary sits under `NAME` (`x-xtrafield` for a header).
#[derive(Debug, Clone, Copy)]
enum Place {
    Query,
    ReqHeader,
    RespHeader,
    Form,
    ReqJson { depth: usize, wrap: bool },
    RespJson { depth: usize, wrap: bool },
    ReqUrl,
    RespUrl,
}

fn place() -> impl Strategy<Value = Place> {
    prop_oneof![
        Just(Place::Query),
        Just(Place::ReqHeader),
        Just(Place::RespHeader),
        Just(Place::Form),
        (0usize..6, any::<bool>()).prop_map(|(depth, wrap)| Place::ReqJson { depth, wrap }),
        (0usize..6, any::<bool>()).prop_map(|(depth, wrap)| Place::RespJson { depth, wrap }),
        Just(Place::ReqUrl),
        Just(Place::RespUrl),
    ]
}

fn json_body(depth: usize, wrap: bool, canary: &str) -> String {
    let leaf = if wrap {
        json!({"inner": canary})
    } else {
        json!(canary)
    };
    let mut body = json!({"keep": "visible", NAME: leaf});
    for _ in 0..depth {
        body = json!({"a": body, "b": 1});
    }
    body.to_string()
}

fn url(canary: &str) -> String {
    format!("https://h.example/cb?{NAME}={canary}&x=1")
}

/// The volatile field the canary lands in, and the strings the sidecar hashed for it.
fn field_of(place: Place, canary: &str) -> (String, String) {
    let json_field = |side: &str, depth: usize, wrap: bool| {
        let mut f = format!("{side}.body/application/json{}/{NAME}", "/a".repeat(depth));
        if wrap {
            f.push_str("/inner");
        }
        f
    };
    match place {
        Place::Query => ("query/xtrafield".into(), canary.into()),
        Place::ReqHeader => ("request.header/x-xtrafield".into(), canary.into()),
        Place::RespHeader => ("response.header/x-xtrafield".into(), canary.into()),
        Place::Form => (
            "request.body/application/x-www-form-urlencoded/xtrafield".into(),
            canary.into(),
        ),
        Place::ReqJson { depth, wrap } => {
            (json_field("request", depth, wrap), format!("\"{canary}\""))
        }
        Place::RespJson { depth, wrap } => {
            (json_field("response", depth, wrap), format!("\"{canary}\""))
        }
        Place::ReqUrl => ("request.header/referer".into(), url(canary)),
        Place::RespUrl => ("response.header/location".into(), url(canary)),
    }
}

fn input_of(place: Place, canary: &str) -> Input {
    let mut i = Input::default();
    match place {
        Place::Query => i.query = format!("{NAME}={canary}&ok=1"),
        Place::ReqHeader => i.req_headers = header("x-xtrafield", canary),
        Place::RespHeader => i.resp_headers = header("x-xtrafield", canary),
        Place::Form => {
            i.req_headers = header("content-type", "application/x-www-form-urlencoded");
            i.req_body = Some(format!("ok=1&{NAME}={canary}"));
        }
        Place::ReqJson { depth, wrap } => {
            i.req_headers = header("content-type", "application/json");
            i.req_body = Some(json_body(depth, wrap, canary));
        }
        Place::RespJson { depth, wrap } => {
            i.resp_headers = header("content-type", "application/json");
            i.resp_body = Some(json_body(depth, wrap, canary));
        }
        Place::ReqUrl => i.req_headers = header("referer", &url(canary)),
        Place::RespUrl => i.resp_headers = header("location", &url(canary)),
    }
    i
}

fn samples_of(r: &Report) -> Vec<(u64, Value)> {
    r.endpoints()
        .iter()
        .flat_map(|e| {
            e.stats
                .samples()
                .iter()
                .map(|(h, s)| (h, serde_json::to_value(s).unwrap()))
        })
        .collect()
}

fn hash_forms(h: u64) -> [String; 3] {
    [h.to_string(), format!("{h:x}"), format!("{h:016x}")]
}

fn controller() -> Redactor {
    Redactor::new([NAME])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// CTL-03: a canary under a name that only the controller knows leaves no trace.
    #[test]
    fn ctl_03_name_unknown_to_the_sidecar(
        place in place(),
        canary in "CANARY[A-Za-z0-9]{8,24}",
        times in 1usize..4,
    ) {
        let input = input_of(place, &canary);
        let mut report = record(&Redactor::default(), &input, times);
        prop_assert!(report.to_json().unwrap().contains(&canary), "the sidecar did not know the name");
        report.re_redact(&controller());
        let json = report.to_json().unwrap();
        prop_assert!(!json.contains(&canary), "canary kept: {json}");
        let (field, hashed) = field_of(place, &canary);
        for text in [canary.as_str(), hashed.as_str()] {
            for form in hash_forms(xxh3_64(text.as_bytes())) {
                prop_assert!(!json.contains(&form), "hash of {text} kept");
            }
        }
        // The sample keys are the hashes of the re-redacted content.
        let truth = record(&controller(), &input, times);
        prop_assert_eq!(samples_of(&report), samples_of(&truth));
        // The field is redacted and has no sketch.
        let fields = report.endpoints()[0].stats.observed().volatility().clone();
        prop_assert!(fields.is_redacted(&field), "{field} not redacted: {:?}", fields.fields());
        prop_assert_eq!(fields.sketch_len(&field), Some(0));
    }

    /// CTL-03: idempotent, and the identity for a report recorded with the same redactor.
    #[test]
    fn ctl_03_idempotent_and_identity(
        place in place(),
        canary in "CANARY[A-Za-z0-9]{8,24}",
        times in 1usize..4,
        known in any::<bool>(),
    ) {
        let sidecar = if known { controller() } else { Redactor::default() };
        let original = record(&sidecar, &input_of(place, &canary), times);
        let mut once = original.clone();
        once.re_redact(&controller());
        let mut twice = once.clone();
        twice.re_redact(&controller());
        prop_assert_eq!(&once, &twice);
        if known {
            prop_assert_eq!(&once, &original);
        }
    }
}

/// Replaces `MARKER` by `secret` in every string value (not key) of `v`.
fn plant(v: &mut Value, secret: &str) {
    match v {
        Value::String(s) if s.contains(MARKER) => *s = s.replace(MARKER, secret),
        Value::Array(items) => items.iter_mut().for_each(|i| plant(i, secret)),
        Value::Object(map) => map.values_mut().for_each(|i| plant(i, secret)),
        _ => {}
    }
}

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJlLXZhbHVl";
const CARD: &str = "4111111111111111";
const GITHUB: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

/// The same exchange with `secret` at each place a value detector applies.
fn detected(secret: &str) -> Vec<(&'static str, Input)> {
    vec![
        (
            "query",
            Input {
                query: format!("q={}", secret.replace('/', "%2F")),
                ..Input::default()
            },
        ),
        (
            "request header",
            Input {
                req_headers: header("x-note", secret),
                ..Input::default()
            },
        ),
        (
            "response header",
            Input {
                resp_headers: header("x-note", secret),
                ..Input::default()
            },
        ),
        (
            "json body",
            Input {
                req_headers: header("content-type", "application/json"),
                req_body: Some(json!({"v": {"w": [secret]}}).to_string()),
                ..Input::default()
            },
        ),
        (
            "form body",
            Input {
                req_headers: header("content-type", "application/x-www-form-urlencoded"),
                resp_body: None,
                req_body: Some(format!("a=1&note={}", secret.replace('/', "%2F"))),
                ..Input::default()
            },
        ),
        (
            "host",
            Input {
                host: Some(secret.to_string()),
                ..Input::default()
            },
        ),
    ]
}

/// CTL-03: a detected form in a report that bypassed the sidecar's redaction is removed.
#[test]
fn ctl_03_detected_forms_in_a_bypassing_report() {
    for secret in [JWT, CARD, GITHUB] {
        for (place, input) in detected(secret) {
            let truth = record(&Redactor::default(), &input, 1);
            let mut value: Value = serde_json::from_str(&truth.to_json().unwrap()).unwrap();
            assert!(
                !value.to_string().contains(secret),
                "{place}: the recorder redacts"
            );
            plant(&mut value, secret);
            let mut report = Report::from_json(&value.to_string()).unwrap();
            assert!(
                report.to_json().unwrap().contains(secret),
                "{place}: planted"
            );
            report.re_redact(&Redactor::default());
            let json = report.to_json().unwrap();
            assert!(!json.contains(secret), "{place}: {secret} kept");
            assert_eq!(samples_of(&report), samples_of(&truth), "{place}");
            assert_eq!(
                report.endpoints()[0].stats.hosts,
                truth.endpoints()[0].stats.hosts
            );
        }
    }
}

/// CTL-03: the concrete path of a sample is redacted again.
#[test]
fn ctl_03_sample_path() {
    let truth = {
        let mut r = Report::new(None);
        r.record(
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 80,
                method: "GET",
                path: &format!("/files/{JWT}"),
                status: 200,
                head_ms: 1,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
        r
    };
    let mut value: Value = serde_json::from_str(&truth.to_json().unwrap()).unwrap();
    let mut samples = 0;
    for (_, s) in samples_of(&truth) {
        assert!(s.to_string().contains(MARKER));
        samples += 1;
    }
    assert_eq!(samples, 1);
    // Plant it in the sample only.
    let key = samples_of(&truth)[0].0;
    let endpoint = value
        .pointer_mut("/scopes/inbound/80")
        .and_then(|scope| find_samples(scope, key))
        .expect("the sample is in the report");
    plant(endpoint, JWT);
    let mut report = Report::from_json(&value.to_string()).unwrap();
    assert!(report.to_json().unwrap().contains(JWT));
    report.re_redact(&Redactor::default());
    assert_eq!(samples_of(&report), samples_of(&truth));
}

fn find_samples(v: &mut Value, key: u64) -> Option<&mut Value> {
    match v {
        Value::Object(map) => {
            if map.contains_key(&key.to_string()) && map.len() == 1 {
                return map.get_mut(&key.to_string());
            }
            map.values_mut().find_map(|i| find_samples(i, key))
        }
        Value::Array(items) => items.iter_mut().find_map(|i| find_samples(i, key)),
        _ => None,
    }
}

/// CTL-03: the volatile field of a detected value in a sample is redacted without sketch.
#[test]
fn ctl_03_detected_form_marks_the_volatile_field() {
    let input = Input {
        req_headers: header("x-note", "plain"),
        ..Input::default()
    };
    let report = record(&Redactor::default(), &input, 1);
    let mut value: Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    plant_value(&mut value, "plain", JWT);
    // The sketch holds the hash of the planted value, as a bypassing sidecar's would.
    let planted = xxh3_64(JWT.as_bytes());
    let plain = xxh3_64(b"plain");
    let text = value
        .to_string()
        .replace(&plain.to_string(), &planted.to_string());
    let mut report = Report::from_json(&text).unwrap();
    assert!(report.to_json().unwrap().contains(&planted.to_string()));
    report.re_redact(&Redactor::default());
    let fields = report.endpoints()[0].stats.observed().volatility().clone();
    assert!(fields.is_redacted("request.header/x-note"));
    assert_eq!(fields.sketch_len("request.header/x-note"), Some(0));
    assert!(!report.to_json().unwrap().contains(&planted.to_string()));
}

fn plant_value(v: &mut Value, from: &str, to: &str) {
    match v {
        Value::String(s) if s == from => *s = to.to_string(),
        Value::Array(items) => items.iter_mut().for_each(|i| plant_value(i, from, to)),
        Value::Object(map) => map.values_mut().for_each(|i| plant_value(i, from, to)),
        _ => {}
    }
}

/// CTL-03: re-redaction does not trust the size a pod declares for a sample.
#[test]
fn ctl_03_sample_size_is_recomputed() {
    let input = input_of(Place::Query, "CANARYabcdefgh");
    let honest = record(&controller(), &input, 1);
    let mut value: Value = serde_json::from_str(&honest.to_json().unwrap()).unwrap();
    let bytes = honest.sample_bytes();
    let text = value
        .to_string()
        .replace(&format!("\"bytes\":{bytes}"), "\"bytes\":1");
    value = serde_json::from_str(&text).unwrap();
    let mut lying = Report::from_json(&value.to_string()).unwrap();
    assert_eq!(lying.sample_bytes(), 1);
    lying.re_redact(&controller());
    assert_eq!(lying, honest);
}

// ---------------------------------------------------------------------------------------
// CTL-04: retention.

/// `GET /e{n}` once per query (distinct samples), then stamped with `day`.
fn stamped(endpoints: &[(usize, &[&str])], day: u64) -> Report {
    let mut r = Report::new(None);
    for (n, queries) in endpoints {
        for query in *queries {
            let path = format!("/e{n}");
            r.record(
                &LIMITS,
                &Redactor::default(),
                &Exchange {
                    direction: Direction::Inbound,
                    port: 80,
                    method: "GET",
                    path: &path,
                    status: 200,
                    head_ms: 1,
                    host: None,
                    grpc: false,
                    observation: Observation {
                        query,
                        ..Observation::default()
                    },
                },
            );
        }
    }
    r.stamp(day);
    r
}

type Days = BTreeMap<String, (Option<u64>, Vec<Option<u64>>)>;

fn days(r: &Report) -> Days {
    r.endpoints()
        .into_iter()
        .map(|e| {
            (
                e.template,
                (
                    e.stats.last_seen(),
                    e.stats
                        .samples()
                        .iter()
                        .map(|(_, s)| s.last_seen())
                        .collect(),
                ),
            )
        })
        .collect()
}

#[test]
fn ctl_04_stamp_sets_every_endpoint_and_sample() {
    let r = stamped(&[(1, &["a=1", "a=2"]), (2, &["b=1"])], 19_000);
    let d = days(&r);
    assert_eq!(d.len(), 2);
    for (endpoint, samples) in d.values() {
        assert_eq!(*endpoint, Some(19_000));
        assert!(!samples.is_empty() && samples.iter().all(|s| *s == Some(19_000)));
    }
}

#[test]
fn ctl_04_stamp_overrides_a_value_sent_by_a_pod() {
    let r = stamped(&[(1, &["a=1"])], 99_999);
    let json = r.to_json().unwrap();
    assert!(json.contains("\"last_seen\":99999"));
    let far = json.replace("99999", "4000000000");
    let mut pushed =
        Report::parse_checked(&far, 256).expect("a report with last_seen passes the check");
    assert_eq!(days(&pushed)["/e1"].0, Some(4_000_000_000));
    pushed.stamp(20_000);
    let d = days(&pushed);
    assert_eq!(d["/e1"], (Some(20_000), vec![Some(20_000)]));
}

#[test]
fn ctl_04_sidecar_report_parses_and_never_serialises_last_seen() {
    let r = record(&Redactor::default(), &Input::default(), 1);
    let json = r.to_json().unwrap();
    assert!(!json.contains("last_seen"));
    let back = Report::parse_checked(&json, 256).expect("parses without last_seen");
    assert_eq!(days(&back)["/p"], (None, vec![None]));
    assert_eq!(back.to_json().unwrap(), json);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn ctl_04_merge_keeps_the_larger_value(d in prop::array::uniform3(0u64..50_000)) {
        let shape: [&[(usize, &[&str])]; 3] = [
            &[(1, &["a=1", "a=2"]), (2, &["b=1"])],
            &[(1, &["a=2", "a=3"]), (3, &["c=1"])],
            &[(2, &["b=1", "b=2"]), (3, &["c=1"])],
        ];
        let reports = [0, 1, 2].map(|i| stamped(shape[i], d[i]));
        let [a, b, c] = reports.clone();
        let ab = merge(a.clone(), b.clone(), &LIMITS);
        prop_assert_eq!(&ab, &merge(b.clone(), a.clone(), &LIMITS));
        let left = merge(ab.clone(), c.clone(), &LIMITS);
        let right = merge(a, merge(b, c.clone(), &LIMITS), &LIMITS);
        prop_assert_eq!(&left, &right);
        prop_assert_eq!(&merge_bounded(ab, c, &LIMITS).report, &left);
        // Each endpoint carries the largest day of the reports that hold it, and each sample
        // the largest day of the reports that hold that sample.
        for (template, (endpoint, samples)) in days(&left) {
            let holders: Vec<u64> = (0..3)
                .filter(|i| days(&reports[*i]).contains_key(&template))
                .map(|i| d[i])
                .collect();
            prop_assert_eq!(endpoint, holders.iter().copied().max());
            prop_assert!(samples.iter().all(Option::is_some));
        }
    }

    /// Dropping removes exactly the endpoints and samples with `last_seen < today - ttl`.
    #[test]
    fn ctl_04_drop_removes_exactly_the_expired(
        old in 0u64..60, new in 0u64..60, today in 0u64..80, ttl in 0u64..40,
    ) {
        let a = stamped(&[(1, &["a=1", "a=2"]), (2, &["b=1"])], old);
        let b = stamped(&[(2, &["b=2"]), (3, &["c=1"])], new);
        let mut r = merge(a, b, &LIMITS);
        let before = days(&r);
        let stale = |d: Option<u64>| d.is_none_or(|d| d + ttl < today);
        let expected_endpoints = before.values().filter(|(e, _)| stale(*e)).count() as u64;
        let expected_samples = before
            .values()
            .flat_map(|(_, s)| s)
            .filter(|d| stale(**d))
            .count() as u64;
        let dropped = drop_expired(&mut r, today, ttl);
        prop_assert_eq!(dropped.get(&Kind::Endpoint).copied().unwrap_or(0), expected_endpoints);
        prop_assert_eq!(dropped.get(&Kind::Sample).copied().unwrap_or(0), expected_samples);
        let after = days(&r);
        for (template, (endpoint, samples)) in &before {
            if stale(*endpoint) {
                prop_assert!(!after.contains_key(template));
            } else {
                let kept: Vec<_> = samples.iter().filter(|d| !stale(**d)).copied().collect();
                prop_assert_eq!(&after[template].1, &kept);
            }
        }
    }
}

#[test]
fn ctl_04_drop_threshold_day_is_kept() {
    let mut r = stamped(&[(1, &["a=1"])], 90);
    // today 100, ttl 10: threshold 90, and `90 < 90` is false.
    assert!(drop_expired(&mut r, 100, 10).is_empty());
    assert_eq!(r.endpoints().len(), 1);
    let dropped = drop_expired(&mut r, 101, 10);
    assert_eq!(
        dropped,
        BTreeMap::from([(Kind::Endpoint, 1), (Kind::Sample, 1)])
    );
    assert!(r.endpoints().is_empty());
    // A ttl longer than the age of the epoch drops nothing.
    let mut r = stamped(&[(1, &["a=1"])], 0);
    assert!(drop_expired(&mut r, 5, 10).is_empty());
}
