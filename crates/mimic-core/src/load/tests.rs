//! LDT-01, LDT-02 and LDT-18: the plan and the request builder.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use proptest::prelude::*;
use serde_json::json;

use super::plan::{CredentialField, Plan, Planned, Spec, build, plan};
use super::request::{Context, Error, Request, request};
use crate::discovery::observed::Observation;
use crate::discovery::redact::{MARKER, Redactor};
use crate::discovery::report::{Direction, Exchange, Limits, Report};

/// A random 32-character canary, only to be found where it must not be.
const CANARY: &str = "q7Zk3vW9xT2mB8nL5cR1dF6hJ4sY0pGa";
/// A random 32-character credential for the Secret.
const SECRET: &str = "Hx5Tn2Qe8Vb4Kd7Ps1Wm9Zc3Ua6Lr0Jf";

fn limits() -> Limits {
    Limits {
        max_literals: 256,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

/// An exchange with owned parts.
#[derive(Debug, Clone)]
struct Ex {
    method: String,
    path: String,
    status: u16,
    query: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    host: Option<String>,
    grpc: bool,
}

impl Ex {
    fn new(method: &str, path: &str, status: u16) -> Self {
        Ex {
            method: method.into(),
            path: path.into(),
            status,
            query: String::new(),
            headers: vec![],
            body: None,
            host: None,
            grpc: false,
        }
    }

    fn query(mut self, q: &str) -> Self {
        self.query = q.into();
        self
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn host(mut self, host: &str) -> Self {
        self.host = Some(host.into());
        self
    }

    fn json(mut self, body: &serde_json::Value) -> Self {
        self.headers
            .push(("content-type".into(), "application/json".into()));
        self.body = Some(body.to_string().into_bytes());
        self
    }

    fn opaque(mut self) -> Self {
        self.headers
            .push(("content-type".into(), "text/plain".into()));
        self.body = Some(b"plain text body".to_vec());
        self
    }

    fn record(&self, r: &mut Report) {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        r.record(
            &limits(),
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port: 8080,
                method: &self.method,
                path: &self.path,
                status: self.status,
                head_ms: 3,
                host: self.host.as_deref(),
                grpc: self.grpc,
                observation: Observation {
                    query: &self.query,
                    req_headers: &headers,
                    req_body: self.body.as_deref(),
                    resp_headers: &[],
                    resp_body: None,
                },
            },
        );
    }
}

fn report(items: &[Ex]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for item in items {
        item.record(&mut r);
    }
    r
}

fn spec() -> Spec {
    Spec {
        include: None,
        exclude: BTreeSet::new(),
        credentials: vec![],
        max_endpoints: 64,
        max_plan: 8 << 20,
    }
}

fn credential(field: &str, key: &str) -> CredentialField {
    CredentialField {
        field: field.into(),
        key: key.into(),
    }
}

fn planned(items: &[Ex], spec: &Spec) -> Planned {
    plan(&report(items), spec)
}

fn keys(p: &Plan) -> Vec<String> {
    p.endpoints
        .iter()
        .map(|e| format!("{} {}", e.method, e.template))
        .collect()
}

fn skipped(p: &Planned) -> Vec<(&'static str, u64)> {
    p.skipped.iter().map(|(k, v)| (*k, *v)).collect()
}

mod plan_tests {
    use super::*;

    #[test]
    fn ldt_01_grpc_upgrade_and_unsampled_endpoints_are_skipped() {
        let mut grpc =
            Ex::new("POST", "/pkg.Svc/Call", 200).header("content-type", "application/grpc");
        grpc.grpc = true;
        let items = [
            Ex::new("GET", "/ok", 200),
            grpc,
            Ex::new("GET", "/ws", 101),
            Ex::new("POST", "/opaque", 200).opaque(),
        ];
        let p = planned(&items, &spec());
        assert_eq!(keys(&p.plan), ["GET /ok"]);
        assert_eq!(
            skipped(&p),
            [("grpc", 1), ("opaque_body", 1), ("upgrade", 1)]
        );

        let mut r = report(&[Ex::new("GET", "/a", 200)]);
        r.drop_samples();
        let p = plan(&r, &spec());
        assert!(p.plan.endpoints.is_empty());
        assert_eq!(skipped(&p), [("no_sample", 1)]);
    }

    #[test]
    fn ldt_01_include_keeps_the_listed_endpoints_and_exclude_wins() {
        let items = [
            Ex::new("GET", "/a", 200),
            Ex::new("POST", "/b", 200),
            Ex::new("GET", "/c", 200),
        ];
        let mut s = spec();
        s.include = Some(["GET /a".to_string(), "POST /b".to_string()].into());
        let p = planned(&items, &s);
        assert_eq!(keys(&p.plan), ["GET /a", "POST /b"]);
        assert_eq!(skipped(&p), [("excluded", 1)]);

        s.exclude = ["POST /b".to_string()].into();
        let p = planned(&items, &s);
        assert_eq!(keys(&p.plan), ["GET /a"]);
        assert_eq!(skipped(&p), [("excluded", 2)]);
    }

    #[test]
    fn ldt_01_success_samples_are_used_when_present_else_errors() {
        let items = [
            Ex::new("GET", "/mixed", 200).query("v=ok"),
            Ex::new("GET", "/mixed", 500).query("v=bad"),
            Ex::new("GET", "/mixed", 302).query("v=redirect"),
            Ex::new("GET", "/failing", 400).query("v=missing"),
            Ex::new("GET", "/failing", 503).query("v=down"),
        ];
        let p = planned(&items, &spec());
        let queries = |template: &str| -> BTreeSet<String> {
            p.plan
                .endpoints
                .iter()
                .filter(|e| e.template == template)
                .flat_map(|e| e.samples.iter().map(|s| s.query[0].1.clone()))
                .collect()
        };
        assert_eq!(
            queries("/mixed"),
            ["ok".to_string(), "redirect".to_string()].into()
        );
        assert_eq!(
            queries("/failing"),
            ["down".to_string(), "missing".to_string()].into()
        );
    }

    #[test]
    fn ldt_01_a_bearer_endpoint_needs_its_credential_field() {
        let items =
            [Ex::new("GET", "/me", 200).header("authorization", &format!("Bearer {CANARY}"))];
        let p = planned(&items, &spec());
        assert!(p.plan.endpoints.is_empty());
        assert_eq!(skipped(&p), [("credentials", 1)]);

        let mut s = spec();
        s.credentials = vec![credential("header/authorization", "token")];
        let p = planned(&items, &s);
        assert_eq!(keys(&p.plan), ["GET /me"]);
        assert!(p.skipped.is_empty());

        // Another field does not help.
        s.credentials = vec![credential("query/api_key", "token")];
        assert_eq!(skipped(&planned(&items, &s)), [("credentials", 1)]);
    }

    #[test]
    fn ldt_01_a_query_or_body_credential_is_supplied_by_its_field() {
        let items = [Ex::new("POST", "/pay", 200)
            .query(&format!("api_key={CANARY}"))
            .json(&json!({"card": {"token": CANARY}, "amount": 5}))];
        let mut s = spec();
        s.credentials = vec![credential("query/api_key", "k")];
        assert_eq!(skipped(&planned(&items, &s)), [("credentials", 1)]);
        s.credentials.push(credential("body/card/token", "t"));
        let p = planned(&items, &s);
        assert_eq!(keys(&p.plan), ["POST /pay"]);
    }

    #[test]
    fn ldt_01_a_redacted_path_segment_is_always_credentials() {
        let items = [Ex::new("GET", &format!("/session/Bearer%20{CANARY}"), 200)];
        let report = report(&items);
        let mut s = spec();
        s.credentials = vec![
            credential("header/authorization", "a"),
            credential("query/api_key", "b"),
            credential("body/token", "c"),
        ];
        let p = plan(&report, &s);
        let json = serde_json::to_string(&report).unwrap_or_default();
        if json.contains(&format!("/{MARKER}")) {
            assert!(p.plan.endpoints.is_empty());
            assert_eq!(skipped(&p), [("credentials", 1)]);
        }
    }

    fn many(n: usize, weight: impl Fn(usize) -> usize) -> Vec<Ex> {
        let mut items = Vec::new();
        for i in 0..n {
            for _ in 0..weight(i) {
                items.push(Ex::new("GET", &format!("/svc{i:02}/list"), 200));
            }
        }
        items
    }

    #[test]
    fn ldt_01_at_most_64_endpoints_with_the_most_exchanges_are_kept() {
        // Endpoints 0..70; the last 6 have one exchange, the others two: the 6 lone ones and
        // none of the others fall out... except that 64 of the 64 heavy ones fit.
        let items = many(70, |i| if i < 64 { 2 } else { 1 });
        let p = planned(&items, &spec());
        assert_eq!(p.plan.endpoints.len(), 64);
        assert_eq!(skipped(&p), [("limit", 6)]);
        assert!(p.plan.endpoints.iter().all(|e| e.weight == 2));

        // Ties go in key order: with equal weights the first 64 keys stay.
        let items = many(70, |_| 1);
        let p = planned(&items, &spec());
        let kept = keys(&p.plan);
        assert_eq!(kept.len(), 64);
        assert_eq!(kept.first().map(String::as_str), Some("GET /svc00/list"));
        assert_eq!(kept.last().map(String::as_str), Some("GET /svc63/list"));
        assert_eq!(skipped(&p), [("limit", 6)]);
    }

    #[test]
    fn ldt_01_weights_equal_exchange_counts() {
        let items = many(3, |i| i + 1);
        let p = planned(&items, &spec());
        let weights: Vec<u64> = p.plan.endpoints.iter().map(|e| e.weight).collect();
        assert_eq!(weights, [1, 2, 3]);
    }

    #[test]
    fn ldt_01_over_the_plan_size_the_highest_hashes_go_first() {
        let mut items = Vec::new();
        for i in 0..8 {
            items.push(Ex::new("GET", "/big", 200).query(&format!("n={i}")));
        }
        items.push(Ex::new("GET", "/small", 200));
        let full = planned(&items, &spec());
        let size = full.plan.to_json().map_or(0, |j| j.len());
        let mut hashes: Vec<u64> = full
            .plan
            .endpoints
            .iter()
            .flat_map(|e| e.samples.iter().map(|s| s.hash))
            .collect();
        hashes.sort_unstable();

        let s = spec();
        let p = build(&report(&items), &s, size - 1);
        let after: BTreeSet<u64> = p
            .plan
            .endpoints
            .iter()
            .flat_map(|e| e.samples.iter().map(|s| s.hash))
            .collect();
        assert!(!after.contains(hashes.last().unwrap_or(&0)));
        assert!(p.plan.to_json().map_or(usize::MAX, |j| j.len()) < size);
        // The survivors are the lowest hashes.
        let kept: Vec<u64> = after.iter().copied().collect();
        assert_eq!(kept, hashes[..kept.len()]);

        // A budget for the skeleton only empties every endpoint.
        let p = build(&report(&items), &s, 2);
        assert!(p.plan.endpoints.is_empty());
        assert_eq!(skipped(&p), [("plan_size", 2)]);
    }

    #[test]
    fn ldt_01_the_same_report_and_spec_give_the_same_bytes_and_no_secret() {
        let items = [
            Ex::new("GET", "/me", 200)
                .header("authorization", &format!("Bearer {CANARY}"))
                .header("x-trace", "abc")
                .host("api.example.com"),
            Ex::new("POST", "/pay", 201).json(&json!({"card": {"token": CANARY}})),
        ];
        let mut s = spec();
        s.credentials = vec![
            credential("header/authorization", "token"),
            credential("body/card/token", "card"),
        ];
        let a = planned(&items, &s).plan.to_json().unwrap_or_default();
        let b = planned(&items, &s).plan.to_json().unwrap_or_default();
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert!(!a.contains(CANARY));
        assert!(a.contains("\"token\"") && a.contains("header/authorization"));
        assert_eq!(
            Plan::from_json(&a, 1 << 20).map(|p| p.to_json().unwrap_or_default()),
            Ok(a.clone())
        );
        assert!(Plan::from_json(&a, a.len() - 1).is_err());
        assert!(Plan::from_json("{", 1 << 20).is_err());
    }

    #[test]
    fn ldt_01_the_host_is_the_first_one_in_byte_order() {
        let items = [
            Ex::new("GET", "/h", 200).host("b.example.com"),
            Ex::new("GET", "/h", 200).host("a.example.com"),
        ];
        let p = planned(&items, &spec());
        assert_eq!(p.plan.endpoints[0].host.as_deref(), Some("a.example.com"));
    }
}

fn ctx<'a>(secrets: &'a BTreeMap<String, String>, now_ms: u64) -> Context<'a> {
    Context {
        marker: "v1.payload.signature",
        ip: IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
        now_ms,
        secrets,
    }
}

fn one(items: &[Ex], spec: &Spec, secrets: &BTreeMap<String, String>) -> Request {
    let p = planned(items, spec);
    request(&p.plan, &ctx(secrets, 1_700_000_000_000), 7).expect("a request")
}

fn header<'a>(r: &'a Request, name: &str) -> Vec<&'a str> {
    r.headers
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

/// The LDT-02 invariants of any request.
fn well_formed(r: &Request) -> bool {
    let marker = |s: &str| s.contains(MARKER);
    let forbidden = |n: &str| {
        matches!(
            n,
            "content-encoding"
                | "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "expect"
        ) || n.starts_with("proxy-")
    };
    !marker(&r.target)
        && !marker(&r.method)
        && !r
            .headers
            .iter()
            .any(|(n, v)| marker(n) || marker(v) || forbidden(n))
        && !String::from_utf8_lossy(&r.body).contains(MARKER)
        && header(r, "mimic-load-run").len() == 1
        && header(r, "host").len() == 1
        && header(r, "content-length")
            .iter()
            .all(|l| l.parse::<usize>() == Ok(r.body.len()))
}

mod request_tests {
    use super::*;

    #[test]
    fn ldt_02_method_path_query_body_and_headers_come_from_the_sample() {
        let items = [Ex::new("POST", "/orders/12", 201)
            .query("a=1&b=x%20y")
            .header("accept", "application/json")
            .header("x-keep", "yes")
            .header("host", "ignored.example.com")
            .header("content-encoding", "gzip")
            .header("connection", "keep-alive")
            .header("keep-alive", "timeout=5")
            .header("te", "trailers")
            .header("upgrade", "h2c")
            .header("expect", "100-continue")
            .header("proxy-connection", "keep-alive")
            .header("content-length", "999")
            .json(&json!({"item": "book", "qty": 2}))];
        let r = one(&items, &spec(), &BTreeMap::new());
        assert_eq!(r.method, "POST");
        assert!(r.target.starts_with("/orders/12?"));
        assert!(r.target.contains("a=1") && r.target.contains("b=x%20y"));
        assert_eq!(r.body, br#"{"item":"book","qty":2}"#);
        assert_eq!(header(&r, "content-length"), ["23"]);
        assert_eq!(header(&r, "x-keep"), ["yes"]);
        assert_eq!(header(&r, "accept"), ["application/json"]);
        assert_eq!(header(&r, "content-type"), ["application/json"]);
        assert_eq!(header(&r, "host"), ["10.1.2.3:8080"]);
        assert_eq!(header(&r, "mimic-load-run"), ["v1.payload.signature"]);
        assert!(well_formed(&r));
    }

    #[test]
    fn ldt_02_host_is_the_endpoints_first_host_else_ip_and_port() {
        let with = [Ex::new("GET", "/h", 200).host("api.example.com")];
        let r = one(&with, &spec(), &BTreeMap::new());
        assert_eq!(header(&r, "host"), ["api.example.com"]);
        let without = [Ex::new("GET", "/h", 200)];
        let r = one(&without, &spec(), &BTreeMap::new());
        assert_eq!(header(&r, "host"), ["10.1.2.3:8080"]);

        let p = planned(&without, &spec());
        let secrets = BTreeMap::new();
        let mut c = ctx(&secrets, 0);
        c.ip = "fd00::1".parse().expect("an address");
        let r = request(&p.plan, &c, 1).expect("a request");
        assert_eq!(header(&r, "host"), ["[fd00::1]:8080"]);
    }

    #[test]
    fn ldt_02_a_form_body_is_encoded_with_its_length() {
        let mut e = Ex::new("POST", "/form", 200);
        e.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        e.body = Some(b"a=1&b=x+y&a=2".to_vec());
        let r = one(&[e], &spec(), &BTreeMap::new());
        let body = String::from_utf8_lossy(&r.body).into_owned();
        assert!(body.contains("a=1") && body.contains("a=2") && body.contains("b=x"));
        assert_eq!(
            header(&r, "content-length"),
            [r.body.len().to_string().as_str()]
        );
    }

    #[test]
    fn ldt_02_the_same_inputs_give_the_same_request() {
        let items = [Ex::new("GET", "/a", 200), Ex::new("GET", "/b", 200)];
        let p = planned(&items, &spec());
        let secrets = BTreeMap::new();
        for seed in 0..50 {
            let a = request(&p.plan, &ctx(&secrets, 5), seed);
            let b = request(&p.plan, &ctx(&secrets, 5), seed);
            assert_eq!(a, b);
        }
    }

    /// Four recordings of a volatile endpoint: the recorder marks the fields volatile.
    fn volatile_items() -> Vec<Ex> {
        let uuids = [
            "550e8400-e29b-41d4-a716-446655440000",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "123e4567-e89b-12d3-a456-426614174000",
        ];
        let trace = [
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-5cf92f3577b34da6a3ce929d0e0e4737-10f067aa0ba902b8-01",
            "00-6df92f3577b34da6a3ce929d0e0e4738-20f067aa0ba902b9-01",
            "00-7ef92f3577b34da6a3ce929d0e0e4739-30f067aa0ba902ba-01",
        ];
        (0..4)
            .map(|i| {
                Ex::new("POST", "/events", 201)
                    .header("x-request-id", uuids[i])
                    .header("traceparent", trace[i])
                    .query(&format!("seq={}", 41_000_000 + i * 7_919))
                    .json(&json!({
                        "at": format!("2026-11-0{}T10:0{}:00Z", i + 1, i),
                        "id": 1_000_000_000_000u64 + (i as u64) * 987_654_321,
                        "note": format!("Ab{i}Zq-{}", 9_000 + i * 13),
                    }))
            })
            .collect()
    }

    fn volatile_request(seed: u64, now_ms: u64) -> Request {
        let p = planned(&volatile_items(), &spec());
        assert_eq!(p.plan.endpoints.len(), 1, "one endpoint");
        for field in [
            "request.header/x-request-id",
            "request.header/traceparent",
            "query/seq",
            "request.body/application/json/at",
            "request.body/application/json/id",
        ] {
            assert!(
                p.plan.endpoints[0].volatile.iter().any(|f| f == field),
                "{field} is volatile"
            );
        }
        let secrets = BTreeMap::new();
        request(&p.plan, &ctx(&secrets, now_ms), seed).expect("a request")
    }

    fn is_hex_uuid(s: &str) -> bool {
        s.len() == 36
            && s.chars().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
            })
    }

    #[test]
    fn ldt_02_volatile_fields_get_fresh_values_of_the_same_shape() {
        // 2023-11-14T22:13:20Z
        let now = 1_700_000_000_000;
        let r = volatile_request(11, now);
        let id = header(&r, "x-request-id")[0];
        assert!(is_hex_uuid(id), "uuid shape");
        let trace = header(&r, "traceparent")[0];
        let parts: Vec<&str> = trace.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [2, 32, 16, 2]
        );
        assert!(trace.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        let seq = r.target.split("seq=").nth(1).unwrap_or_default();
        assert_eq!(seq.len(), 8, "{}", r.target);
        assert!(seq.chars().all(|c| c.is_ascii_digit()) && !seq.starts_with('0'));

        let body: serde_json::Value = serde_json::from_slice(&r.body).expect("json");
        assert_eq!(body["at"], "2023-11-14T22:13:20Z");
        let digits = body["id"].as_u64().map(|n| n.to_string().len());
        assert_eq!(digits, Some(13));
        let note = body["note"].as_str().unwrap_or_default();
        assert_eq!(note.len(), 10);
        assert_eq!(note.as_bytes()[5], b'-');
    }

    #[test]
    fn ldt_02_two_random_inputs_give_different_volatile_values() {
        let a = volatile_request(1, 1_700_000_000_000);
        let b = volatile_request(2, 1_700_000_000_000);
        assert_ne!(header(&a, "x-request-id"), header(&b, "x-request-id"));
        assert_ne!(header(&a, "traceparent"), header(&b, "traceparent"));
        assert_ne!(a.target, b.target);
        let ids = |r: &Request| {
            serde_json::from_slice::<serde_json::Value>(&r.body).map(|v| v["id"].clone())
        };
        assert_ne!(ids(&a).ok(), ids(&b).ok());
        assert_eq!(volatile_request(1, 1_700_000_000_000), a);
    }

    #[test]
    fn ldt_02_credentials_replace_the_whole_value_without_the_trailing_newline() {
        let items = [Ex::new("POST", "/pay", 200)
            .header("authorization", &format!("Bearer {CANARY}"))
            .query(&format!("api_key={CANARY}&x=1"))
            .json(&json!({"card": {"token": CANARY}, "amount": 5}))];
        let mut s = spec();
        s.credentials = vec![
            credential("header/authorization", "auth"),
            credential("query/api_key", "key"),
            credential("body/card/token", "card"),
        ];
        let secrets: BTreeMap<String, String> = [
            ("auth".to_string(), format!("Bearer {SECRET}\n")),
            ("key".to_string(), format!("{SECRET}k\n")),
            ("card".to_string(), format!("{SECRET}c")),
        ]
        .into();
        let r = one(&items, &s, &secrets);
        assert_eq!(header(&r, "authorization"), [format!("Bearer {SECRET}")]);
        assert!(
            r.target.contains(&format!("api_key={SECRET}k&"))
                || r.target.ends_with(&format!("api_key={SECRET}k"))
        );
        let body: serde_json::Value = serde_json::from_slice(&r.body).expect("json");
        assert_eq!(body["card"]["token"], format!("{SECRET}c"));
        assert_eq!(body["amount"], 5);
        assert!(well_formed(&r));
        assert!(!String::from_utf8_lossy(&r.body).contains(CANARY));
    }

    #[test]
    fn ldt_02_a_missing_secret_key_is_an_error_without_the_value() {
        let items = [Ex::new("GET", "/me", 200).header("authorization", "Bearer abc")];
        let mut s = spec();
        s.credentials = vec![credential("header/authorization", "auth")];
        let p = planned(&items, &s);
        let secrets = BTreeMap::new();
        let err = request(&p.plan, &ctx(&secrets, 0), 1);
        assert_eq!(err, Err(Error::MissingSecret("auth".into())));
    }

    #[test]
    fn ldt_02_a_header_value_with_a_line_break_is_refused() {
        let items = [Ex::new("GET", "/me", 200).header("authorization", "Bearer abc")];
        let mut s = spec();
        s.credentials = vec![credential("header/authorization", "auth")];
        let p = planned(&items, &s);
        let secrets: BTreeMap<String, String> =
            [("auth".to_string(), format!("x\r\nInjected: {CANARY}"))].into();
        let err = request(&p.plan, &ctx(&secrets, 0), 1);
        assert!(matches!(err, Err(Error::Invalid(_))));
        assert!(!format!("{err:?}").contains(CANARY));
    }

    #[test]
    fn ldt_02_an_empty_plan_or_a_marker_in_the_plan_builds_nothing() {
        let secrets = BTreeMap::new();
        assert_eq!(
            request(&Plan::default(), &ctx(&secrets, 0), 1),
            Err(Error::Empty)
        );
        let items = [Ex::new("GET", "/me", 200).header("x-a", "b")];
        let mut p = planned(&items, &spec()).plan;
        p.endpoints[0].samples[0]
            .headers
            .push(("x-leak".into(), format!("Bearer {MARKER}")));
        assert_eq!(request(&p, &ctx(&secrets, 0), 1), Err(Error::Redacted));
    }

    #[test]
    fn ldt_02_endpoint_draws_follow_the_weights() {
        let mut items = Vec::new();
        for (i, weight) in [1usize, 2, 3, 4].into_iter().enumerate() {
            for _ in 0..weight {
                items.push(Ex::new("GET", &format!("/w{i}/list"), 200));
            }
        }
        let p = planned(&items, &spec());
        let secrets = BTreeMap::new();
        let n = 20_000u64;
        let mut seen = [0u64; 4];
        for seed in 0..n {
            let r = request(&p.plan, &ctx(&secrets, 0), seed).expect("a request");
            seen[r.endpoint] += 1;
        }
        let total: f64 = 10.0;
        let chi2: f64 = [1.0, 2.0, 3.0, 4.0]
            .iter()
            .zip(seen)
            .map(|(w, got)| {
                let expected = n as f64 * w / total;
                (got as f64 - expected).powi(2) / expected
            })
            .sum();
        // Critical value of chi-squared with 3 degrees of freedom at 1 %.
        assert!(chi2 < 11.34, "chi2 = {chi2}");
    }

    fn arb_exchange() -> impl Strategy<Value = Ex> {
        let paths = prop::sample::select(vec!["/a", "/a/1", "/b/list", "/c/42/items", "/d"]);
        let methods = prop::sample::select(vec!["GET", "POST", "PUT", "DELETE"]);
        let statuses = prop::sample::select(vec![200u16, 201, 302, 404, 500, 101]);
        let headers = prop::collection::vec(
            (
                prop::sample::select(vec![
                    "authorization",
                    "x-api-key",
                    "cookie",
                    "x-id",
                    "accept",
                    "content-encoding",
                    "proxy-authorization",
                ]),
                prop::sample::select(vec![
                    format!("Bearer {CANARY}"),
                    CANARY.to_string(),
                    "plain".to_string(),
                    format!("sid={CANARY}"),
                    "7f3a9c1e-1111-2222-3333-444455556666".to_string(),
                ]),
            ),
            0..4,
        );
        let query = prop::sample::select(vec![
            String::new(),
            format!("api_key={CANARY}"),
            "page=2&sort=asc".to_string(),
            format!("token={CANARY}&q=1"),
        ]);
        let body = prop::sample::select(vec![
            None,
            Some(json!({"card": {"token": CANARY}, "n": 1})),
            Some(json!({"items": [{"id": 1}, {"id": 2}], "when": "2026-11-01"})),
            Some(json!("scalar")),
        ]);
        (paths, methods, statuses, headers, query, body).prop_map(
            |(path, method, status, headers, query, body)| {
                let mut e = Ex::new(method, path, status).query(&query);
                for (n, v) in headers {
                    e = e.header(n, &v);
                }
                match body {
                    Some(b) => e.json(&b),
                    None => e,
                }
            },
        )
    }

    fn arb_spec() -> impl Strategy<Value = Spec> {
        let fields = prop::sample::subsequence(
            vec![
                "header/authorization",
                "header/x-api-key",
                "header/cookie",
                "query/api_key",
                "query/token",
                "body/card/token",
            ],
            0..=6,
        );
        (fields, prop::sample::select(vec![1usize, 2, 64])).prop_map(|(fields, max_endpoints)| {
            Spec {
                credentials: fields.into_iter().map(|f| credential(f, "k")).collect(),
                max_endpoints,
                ..spec()
            }
        })
    }

    /// Every request of any recorded report and spec is well formed, holds no marker and takes
    /// its non-volatile, non-credential values from the drawn sample.
    fn check_property(items: &[Ex], spec: &Spec, seed: u64) {
        let p = plan(&report(items), spec);
        let secrets: BTreeMap<String, String> = [("k".to_string(), SECRET.to_string())].into();
        let Ok(r) = request(&p.plan, &ctx(&secrets, 1_700_000_000_000), seed) else {
            return;
        };
        assert!(well_formed(&r), "a well-formed request");
        let endpoint = &p.plan.endpoints[r.endpoint];
        let sample = &endpoint.samples[r.sample];
        assert_eq!(r.method, endpoint.method);
        let supplied: BTreeSet<&str> = spec.credentials.iter().map(|c| c.field.as_str()).collect();
        let volatile = |field: String| endpoint.volatile.contains(&field);
        for (name, value) in &r.headers {
            if matches!(name.as_str(), "host" | "content-length" | "mimic-load-run") {
                continue;
            }
            let field = format!("header/{name}");
            if supplied.contains(field.as_str()) || volatile(format!("request.header/{name}")) {
                continue;
            }
            assert!(
                sample.headers.iter().any(|(n, v)| n == name && v == value),
                "header {name} comes from the sample"
            );
        }
        for pair in r.target.split_once('?').map_or("", |(_, q)| q).split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            if supplied.contains(format!("query/{k}").as_str()) || volatile(format!("query/{k}")) {
                continue;
            }
            assert!(
                sample.query.iter().any(|(sk, sv)| sk == k && sv == v),
                "query value of {k} comes from the sample"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        #[test]
        fn ldt_02_every_request_is_well_formed_and_comes_from_its_sample(
            items in prop::collection::vec(arb_exchange(), 1..6),
            spec in arb_spec(),
            seed in any::<u64>(),
        ) {
            check_property(&items, &spec, seed);
        }
    }
}

mod robustness {
    use super::*;

    /// The recorded report the corruption starts from.
    fn seed_json() -> String {
        let items = [
            Ex::new("GET", "/me", 200)
                .header("authorization", &format!("Bearer {CANARY}"))
                .host("api.example.com"),
            Ex::new("POST", "/pay", 201)
                .query(&format!("api_key={CANARY}"))
                .json(&json!({"card": {"token": CANARY}, "at": "2026-11-01T10:00:00Z"})),
            Ex::new("GET", "/orders/1", 200),
            Ex::new("GET", "/orders/2", 200),
            Ex::new("GET", "/orders/3", 200),
        ];
        report(&items).to_json().expect("a report")
    }

    #[test]
    fn ldt_18_plans_of_corrupted_reports_build_well_formed_requests_or_none() {
        let seed = seed_json();
        let bytes = seed.as_bytes();
        let secrets: BTreeMap<String, String> = [("k".to_string(), SECRET.to_string())].into();
        let specs = [
            spec(),
            Spec {
                credentials: vec![
                    credential("header/authorization", "k"),
                    credential("query/api_key", "k"),
                    credential("body/card/token", "k"),
                ],
                ..spec()
            },
            Spec {
                max_endpoints: 1,
                max_plan: 700,
                ..spec()
            },
        ];
        let mut accepted = 0;
        for at in (0..bytes.len()).step_by(3) {
            for &value in b"0\"</}" {
                let mut mutated = bytes.to_vec();
                mutated[at] = value;
                let Ok(report) = Report::from_json(&String::from_utf8_lossy(&mutated)) else {
                    continue;
                };
                accepted += 1;
                for spec in &specs {
                    let p = build(&report, spec, spec.max_plan);
                    assert!(p.plan.to_json().map_or(0, |j| j.len()) <= spec.max_plan.max(64));
                    for s in 0..4 {
                        if let Ok(r) = request(&p.plan, &ctx(&secrets, 0), s) {
                            assert!(well_formed(&r));
                        }
                    }
                }
            }
        }
        assert!(accepted > 100, "{accepted} corrupted reports accepted");
    }
}

/// Follow-ups of the review of step 1: names with the marker, `Debug` without values, the plan
/// limit's floor.
mod follow_ups {
    use super::*;
    use crate::load::plan::{MIN_PLAN, plan_limit};

    /// The report of `items` with `from` replaced by `to` in its JSON.
    fn marked(items: &[Ex], from: &str, to: &str) -> Report {
        let json = report(items).to_json().expect("a report");
        assert!(json.contains(from), "{from} not in the report");
        Report::from_json(&json.replace(from, to)).expect("a report")
    }

    fn all_fields() -> Spec {
        let mut s = spec();
        for field in [
            "header/<redacted>",
            "header/x-custom-name",
            "query/<redacted>",
            "query/qkey",
            "body/<redacted>",
            "body/jsonkey",
            "body/n/<redacted>",
            "body/fkey",
        ] {
            s.credentials.push(credential(field, "k"));
        }
        s
    }

    #[test]
    fn ldt_01_a_marker_in_a_header_query_form_or_json_key_is_credentials() {
        let named = Ex::new("POST", "/a", 200)
            .header("x-custom-name", "v1")
            .query("qkey=1")
            .json(&json!({"jsonkey": 1, "n": {"deep": 2}}));
        let mut form =
            Ex::new("POST", "/f", 200).header("content-type", "application/x-www-form-urlencoded");
        form.body = Some(b"fkey=1&g=2".to_vec());
        let cases = [
            (
                vec![named.clone()],
                r#"[["x-custom-name","v1"]"#,
                r#"[["<redacted>","v1"]"#,
            ),
            (
                vec![named.clone()],
                r#""query":[["qkey""#,
                r#""query":[["<redacted>""#,
            ),
            (
                vec![named.clone()],
                r#""value":{"jsonkey""#,
                r#""value":{"<redacted>""#,
            ),
            (vec![named], r#"{"deep":2}"#, r#"{"<redacted>":2}"#),
            (vec![form], r#"[["fkey","1"]"#, r#"[["<redacted>","1"]"#),
        ];
        for (items, from, to) in cases {
            // Unmarked, the endpoint is planned.
            let clean = plan(&report(&items), &all_fields());
            assert_eq!(clean.plan.endpoints.len(), 1, "{from}");
            // Marked: skipped at plan time, whatever the spec supplies.
            let marked = marked(&items, from, to);
            for s in [spec(), all_fields()] {
                let p = plan(&marked, &s);
                assert!(p.plan.endpoints.is_empty(), "{from}");
                assert_eq!(skipped(&p), [("credentials", 1)], "{from}");
            }
        }
    }

    #[test]
    fn ldt_01_context_and_request_debug_print_names_only() {
        let secrets: BTreeMap<String, String> = [("token".to_string(), SECRET.to_string())].into();
        let c = ctx(&secrets, 5);
        let shown = format!("{c:?}");
        assert!(shown.contains("token"), "{shown}");
        assert!(
            !shown.contains(SECRET) && !shown.contains(c.marker),
            "{shown}"
        );

        let mut s = spec();
        s.credentials = vec![
            credential("header/authorization", "token"),
            credential("query/api_key", "token"),
        ];
        let items = [Ex::new("GET", "/me", 200)
            .header("authorization", &format!("Bearer {CANARY}"))
            .query(&format!("api_key={CANARY}"))];
        let r = one(&items, &s, &secrets);
        assert_eq!(header(&r, "authorization"), [SECRET]);
        let shown = format!("{r:?}");
        assert!(
            shown.contains("authorization") && shown.contains("GET"),
            "{shown}"
        );
        for secret in [SECRET, CANARY, ctx(&secrets, 5).marker, "api_key"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        // Errors never hold a value either.
        let e = format!("{:?}", Error::MissingSecret("token".into()));
        assert!(!e.contains(SECRET));
    }

    #[test]
    fn ldt_01_the_plan_limit_has_a_floor_that_holds_the_empty_plan() {
        assert_eq!(MIN_PLAN, 64 << 10);
        assert!(Plan::default().to_json().expect("json").len() < 64);
        assert_eq!(plan_limit("64Ki"), Ok(64 << 10));
        assert_eq!(plan_limit("65536"), Ok(64 << 10));
        assert_eq!(plan_limit("8Mi"), Ok(8 << 20));
        assert_eq!(plan_limit("256Mi"), Ok(256 << 20));
        for bad in [
            "63Ki", "65535", "34", "0", "", "257Mi", "1Gi", "-1", "8 Mi", "1.5Mi", "8MiB", "Mi",
        ] {
            assert!(plan_limit(bad).is_err(), "{bad}");
        }
        // A plan shrunk to the floor is readable.
        let mut items = Vec::new();
        for i in 0..40 {
            items.push(
                Ex::new("GET", "/big", 200).query(&format!("n={i}&pad={}", "x".repeat(4000))),
            );
        }
        let mut s = spec();
        s.max_plan = MIN_PLAN;
        let p = planned(&items, &s);
        let json = p.plan.to_json().expect("json");
        assert!(json.len() <= MIN_PLAN);
        assert!(Plan::from_json(&json, MIN_PLAN).is_ok());
        // Even when everything is dropped.
        let empty = Plan::default().to_json().expect("json");
        assert!(Plan::from_json(&empty, MIN_PLAN).is_ok());
    }

    #[test]
    fn ldt_01_a_plan_never_exceeds_the_floor_of_a_spec_limit_below_it() {
        let items = [Ex::new("GET", "/me", 200)];
        let mut s = spec();
        s.max_plan = 2;
        let p = planned(&items, &s);
        // Raised to MIN_PLAN: the endpoint is kept and the plan reads back.
        assert!(!p.plan.endpoints.is_empty());
        let json = p.plan.to_json().expect("json");
        assert!(Plan::from_json(&json, MIN_PLAN).is_ok());
        assert!(p.skipped.is_empty());
    }
}
