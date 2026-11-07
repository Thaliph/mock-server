//! MCK-02 to MCK-06: matching, tiers, answer, latency and signature of the mock (spec §8).
//! Canaries never appear in assertion messages.

use std::collections::VecDeque;
use std::net::SocketAddr;

use proptest::prelude::*;

use crate::Mode;
use crate::compact::SortedMap;
use crate::discovery::egress::EgressSet;
use crate::discovery::histogram::{Histogram, bucket_index};
use crate::discovery::observed::Observation;
use crate::discovery::redact::Redactor;
use crate::discovery::report::{Direction, Exchange, Limits, Report, Via};
use crate::discovery::sample::Payload;
use crate::discovery::scope::ScopeKey;

use super::*;

const AUTH: &str = "api.example.com:443";
const JSON: &str = "application/json";
const FORM: &str = "application/x-www-form-urlencoded";

// --- sets built by the recorder -------------------------------------------------------

/// One exchange to record.
#[derive(Debug, Clone)]
struct Ex {
    method: String,
    path: String,
    query: String,
    req_headers: Vec<(String, String)>,
    req_body: Option<Vec<u8>>,
    status: u16,
    resp_headers: Vec<(String, String)>,
    resp_body: Option<Vec<u8>>,
    grpc: bool,
    head_ms: u64,
}

fn ex(method: &str, path: &str, status: u16) -> Ex {
    Ex {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        req_headers: Vec::new(),
        req_body: None,
        status,
        resp_headers: Vec::new(),
        resp_body: None,
        grpc: false,
        head_ms: 5,
    }
}

impl Ex {
    fn query(mut self, q: &str) -> Self {
        self.query = q.into();
        self
    }

    fn req_json(mut self, body: &str) -> Self {
        self.req_headers.push(("content-type".into(), JSON.into()));
        self.req_body = Some(body.as_bytes().to_vec());
        self
    }

    fn resp(mut self, ct: &str, body: &str) -> Self {
        self.resp_headers.push(("content-type".into(), ct.into()));
        self.resp_body = Some(body.as_bytes().to_vec());
        self
    }

    fn resp_header(mut self, name: &str, value: &str) -> Self {
        self.resp_headers.push((name.into(), value.into()));
        self
    }
}

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

fn record_in(authority: &str, exchanges: &[Ex]) -> EgressSet {
    let mut report = Report::new(None);
    for e in exchanges {
        let borrow = |h: &[(String, String)]| -> Vec<(String, String)> { h.to_vec() };
        let (req, resp) = (borrow(&e.req_headers), borrow(&e.resp_headers));
        let req: Vec<(&str, &str)> = req.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        let resp: Vec<(&str, &str)> = resp.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        report.record_at(
            ScopeKey::Authority(authority.into()),
            &PLAIN,
            &limits(),
            &Redactor::default(),
            &Exchange {
                direction: Direction::Outbound,
                port: 8080,
                method: &e.method,
                path: &e.path,
                status: e.status,
                head_ms: e.head_ms,
                host: None,
                grpc: e.grpc,
                observation: Observation {
                    query: &e.query,
                    req_headers: &req,
                    req_body: e.req_body.as_deref(),
                    resp_headers: &resp,
                    resp_body: e.resp_body.as_deref(),
                },
            },
        );
    }
    EgressSet::project(&report)
}

fn record(exchanges: &[Ex]) -> EgressSet {
    record_in(AUTH, exchanges)
}

fn repeat(e: Ex, times: usize) -> Vec<Ex> {
    vec![e; times]
}

// --- requests ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    method: String,
    target: Option<String>,
    tls: bool,
    sni: Option<String>,
    dst: SocketAddr,
    h2: bool,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

fn req(method: &str, path: &str) -> Req {
    Req {
        method: method.into(),
        target: Some("api.example.com".into()),
        tls: true,
        sni: None,
        dst: "10.0.0.5:443".parse().expect("an address"),
        h2: false,
        path: path.into(),
        query: String::new(),
        headers: Vec::new(),
        body: None,
    }
}

impl Req {
    fn query(mut self, q: &str) -> Self {
        self.query = q.into();
        self
    }

    fn json(mut self, body: &str) -> Self {
        self.headers.push(("content-type".into(), JSON.into()));
        self.body = Some(body.as_bytes().to_vec());
        self
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn with<R>(&self, f: impl FnOnce(&Request<'_>) -> R) -> R {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        f(&Request {
            method: &self.method,
            target: self.target.as_deref(),
            tls: self.tls,
            sni: self.sni.as_deref(),
            dst: self.dst,
            h2: self.h2,
            path: &self.path,
            query: &self.query,
            headers: &headers,
            body: self.body.as_deref(),
        })
    }
}

/// Looks up and renders the answer: `(tier, response)` or the miss.
fn ask(index: &Index, r: &Req) -> Result<(Tier, Response), Miss> {
    r.with(|q| index.lookup(&Redactor::default(), q))
        .map(|hit| (hit.tier, hit.response(&r.method)))
}

fn index(set: &EgressSet) -> Index {
    Index::build(set.clone())
}

fn status(index: &Index, r: &Req) -> Result<u16, Miss> {
    ask(index, r).map(|(_, response)| response.status)
}

fn text(response: &Response) -> String {
    String::from_utf8_lossy(response.body()).into_owned()
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

// --- MCK-02 -----------------------------------------------------------------------------

#[test]
fn mck_02_an_authority_absent_from_the_set_is_a_miss() {
    let empty = EgressSet {
        authorities: SortedMap::new(),
        format: 1,
    };
    let r = req("GET", "/users/42");
    assert_eq!(status(&index(&empty), &r), Err(Miss::Authority));
    let other = record_in("other.example.com:443", &[ex("GET", "/users/42", 200)]);
    assert_eq!(status(&index(&other), &r), Err(Miss::Authority));
}

#[test]
fn mck_02_a_literal_beats_a_parameter_whatever_the_order() {
    let mut set = record(&[ex("GET", "/users/42", 200), ex("GET", "/users/me", 201)]);
    for reversed in [false, true] {
        if reversed {
            set.authorities
                .get_mut(AUTH)
                .expect("the authority")
                .endpoints
                .reverse();
        }
        let index = index(&set);
        assert_eq!(status(&index, &req("GET", "/users/42")), Ok(200));
        assert_eq!(status(&index, &req("GET", "/users/7")), Ok(200));
        assert_eq!(status(&index, &req("GET", "/users/me")), Ok(201));
    }
}

#[test]
fn mck_02_the_method_and_the_depth_must_match() {
    let index = index(&record(&[ex("GET", "/users/42", 200)]));
    for r in [
        req("HEAD", "/users/42"),
        req("POST", "/users/42"),
        req("GET", "/users/42/x"),
        req("GET", "/users"),
    ] {
        assert_eq!(
            status(&index, &r),
            Err(Miss::Endpoint),
            "{} {}",
            r.method,
            r.path
        );
    }
}

#[test]
fn mck_02_a_grpc_request_takes_only_the_grpc_endpoint_of_its_exact_path() {
    let mut call = ex("POST", "/pkg.Svc/Get", 200).resp("application/grpc", "");
    call.grpc = true;
    let index = index(&record(&[call]));
    let mut grpc = req("POST", "/pkg.Svc/Get").header("content-type", "application/grpc");
    grpc.h2 = true;
    assert_eq!(status(&index, &grpc), Ok(200));
    let mut proto = req("POST", "/pkg.Svc/Get").header("content-type", "application/grpc+proto");
    proto.h2 = true;
    assert_eq!(status(&index, &proto), Ok(200));
    let mut other = req("POST", "/pkg.Svc/Other").header("content-type", "application/grpc");
    other.h2 = true;
    assert_eq!(status(&index, &other), Err(Miss::Endpoint));
    // Not gRPC (HTTP/1, or another content type): the grpc endpoint does not answer it.
    assert_eq!(
        status(&index, &req("POST", "/pkg.Svc/Get")),
        Err(Miss::Endpoint)
    );
    let plain = req("POST", "/pkg.Svc/Get").header("content-type", JSON);
    assert_eq!(status(&index, &plain), Err(Miss::Endpoint));
}

#[test]
fn mck_02_the_authority_is_the_scope_key() {
    let at_80 = index(&record_in("api.example.com:80", &[ex("GET", "/ping", 200)]));
    let mut cleartext = req("GET", "/ping");
    cleartext.target = Some("API.Example.COM.".into());
    cleartext.tls = false;
    cleartext.dst = "10.0.0.9:80".parse().expect("an address");
    assert_eq!(status(&at_80, &cleartext), Ok(200));

    let at_8443 = index(&record_in(
        "api.example.com:8443",
        &[ex("GET", "/ping", 200)],
    ));
    let mut sni = req("GET", "/ping");
    sni.target = None;
    sni.sni = Some("api.example.com".into());
    sni.dst = "10.0.0.9:8443".parse().expect("an address");
    assert_eq!(status(&at_8443, &sni), Ok(200));
}

#[test]
fn mck_02_a_path_segment_holding_a_jwt_takes_the_parameter() {
    let index = index(&record(&[ex("GET", "/users/42", 200)]));
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
    assert_eq!(
        status(&index, &req("GET", &format!("/users/{jwt}"))),
        Ok(200)
    );
}

// --- MCK-03 -----------------------------------------------------------------------------

/// Five orders: the query `ts` changes at each (volatile), the token is redacted by name,
/// `memo` holds a card number in the first sample only, `item` is `a` or `b`.
fn orders() -> EgressSet {
    let sample = |i: usize, item: &str, memo: &str| {
        ex("POST", "/orders", 200 + i as u16)
            .query(&format!("ts={i}"))
            .req_json(&format!(
                r#"{{"item":"{item}","token":"tok-{i}","memo":"{memo}"}}"#
            ))
            .resp(JSON, &format!(r#"{{"id":{i}}}"#))
    };
    record(&[
        sample(1, "a", "4111111111111111"),
        sample(2, "a", "hello"),
        sample(3, "a", "hello"),
        sample(4, "b", "hello"),
        sample(5, "b", "hello"),
    ])
}

/// The id of the first sample (in set order) whose request body holds `needle`.
fn first_sample_with(set: &EgressSet, needle: &str) -> u16 {
    let endpoint = &set.authorities.get(AUTH).expect("authority").endpoints[0];
    endpoint
        .samples
        .iter()
        .find(|s| matches!(s.request_payload(), Some((_, Payload::Json(t))) if t.contains(needle)))
        .map(|s| s.status())
        .expect("a sample")
}

#[test]
fn mck_03_volatile_redacted_and_header_values_do_not_count() {
    let set = orders();
    let index = index(&set);
    let r = req("POST", "/orders")
        .query("ts=9999")
        .json(r#"{"item":"b","token":"other","memo":"hi"}"#)
        .header("x-request-id", "something-else")
        .header("authorization", "Bearer nope");
    let (tier, response) = ask(&index, &r).expect("an answer");
    assert_eq!(tier, Tier::Exact);
    assert_eq!(response.status, first_sample_with(&set, r#""item":"b""#));
    assert_eq!(header(&response, "x-mimic-match"), Some("exact"));
}

#[test]
fn mck_03_one_differing_leaf_is_the_endpoint_tier_with_the_most_shared_fields() {
    // Three samples, each three times: two values per field at most, nothing is volatile.
    let sample = |status: u16, item: &str, qty: u32, color: &str| {
        repeat(
            ex("POST", "/lines", status)
                .req_json(&format!(
                    r#"{{"item":"{item}","qty":{qty},"color":"{color}"}}"#
                ))
                .resp(JSON, &format!(r#"{{"s":{status}}}"#)),
            3,
        )
    };
    let set = record(
        &[
            sample(200, "a", 1, "red"),
            sample(201, "b", 2, "red"),
            sample(202, "b", 3, "blue"),
        ]
        .concat(),
    );
    let index = index(&set);
    let ask_body = |body: &str| ask(&index, &req("POST", "/lines").json(body));
    let (tier, r) = ask_body(r#"{"item":"b","qty":9,"color":"blue"}"#).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Endpoint, 202));
    // A tie goes to the first in set order.
    let (tier, r) = ask_body(r#"{"item":"b","qty":9,"color":"green"}"#).expect("an answer");
    let endpoint = &set.authorities.get(AUTH).expect("authority").endpoints[0];
    let first = endpoint
        .samples
        .iter()
        .map(|s| s.status())
        .find(|s| [201, 202].contains(s))
        .expect("a sample");
    assert_eq!((tier, r.status), (Tier::Endpoint, first));
    // No shared field: the most frequent status, the lowest on a tie.
    let (tier, r) = ask_body(r#"{"item":"z","qty":9,"color":"green"}"#).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Fallback, 200));
    assert_eq!(header(&r, "x-mimic-match"), Some("fallback"));
}

#[test]
fn mck_03_the_fallback_is_the_most_frequent_status() {
    let mut exchanges = repeat(ex("GET", "/s", 200).resp(JSON, r#"{"ok":true}"#), 5);
    exchanges.extend(repeat(
        ex("GET", "/s", 404).resp(JSON, r#"{"ok":false}"#),
        5,
    ));
    exchanges.extend(repeat(ex("GET", "/t", 500).resp(JSON, "{}"), 1));
    exchanges.extend(repeat(ex("GET", "/t", 502).resp(JSON, "{}"), 3));
    let index = index(&record(&exchanges));
    let (tier, r) = ask(&index, &req("GET", "/s").query("zzz=1")).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Fallback, 200));
    assert_eq!(text(&r), r#"{"ok":true}"#);
    let (_, r) = ask(&index, &req("GET", "/t").query("zzz=1")).expect("an answer");
    assert_eq!(r.status, 502);
}

#[test]
fn mck_03_a_status_without_sample_is_answered_with_an_empty_body() {
    let mut exchanges = repeat(ex("GET", "/x", 200).resp(JSON, r#"{"a":1}"#), 5);
    exchanges.push(ex("GET", "/x", 500).resp(JSON, r#"{"a":2}"#));
    let mut set = record(&exchanges);
    set.authorities
        .get_mut(AUTH)
        .expect("authority")
        .endpoints
        .iter_mut()
        .for_each(|e| e.samples.retain(|s| s.status() != 200));
    let index = index(&set);
    let (tier, r) = ask(&index, &req("GET", "/x").query("zzz=1")).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Fallback, 200));
    assert!(r.body().is_empty());
    assert_eq!(header(&r, "content-length"), Some("0"));
    assert_eq!(header(&r, "x-mimic-match"), Some("fallback"));
}

#[test]
fn mck_03_a_1xx_sample_or_status_is_never_used() {
    let only = index(&record(&repeat(ex("GET", "/ws", 101), 3)));
    assert_eq!(status(&only, &req("GET", "/ws")), Err(Miss::Endpoint));

    let mut exchanges = repeat(ex("GET", "/ws", 101), 5);
    exchanges.push(ex("GET", "/ws", 200).resp(JSON, r#"{"ok":1}"#));
    let both = index(&record(&exchanges));
    let (tier, r) = ask(&both, &req("GET", "/ws")).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Exact, 200));
    let (tier, r) = ask(&both, &req("GET", "/ws").query("zzz=1")).expect("an answer");
    assert_eq!((tier, r.status), (Tier::Fallback, 200));
}

#[test]
fn mck_03_the_result_is_deterministic_and_ignores_the_order_of_query_keys() {
    let sample = |a: &str, b: &str, status: u16| {
        repeat(ex("GET", "/q", status).query(&format!("a={a}&b={b}")), 3)
    };
    let set = record(
        &[
            sample("1", "x", 200),
            sample("2", "y", 201),
            sample("1", "y", 202),
        ]
        .concat(),
    );
    let (first, second) = (index(&set), index(&set));
    for q in ["a=1&b=y", "b=y&a=1", "a=2&b=z", "b=z&a=2", "a=1&a=2&b=y"] {
        let r = req("GET", "/q").query(q);
        let one = ask(&first, &r).map(|(t, r)| (t, r.status));
        assert_eq!(one, ask(&second, &r).map(|(t, r)| (t, r.status)), "{q}");
    }
    let key = |q: &str| ask(&first, &req("GET", "/q").query(q)).map(|(t, r)| (t, r.status));
    assert_eq!(key("a=1&b=y"), key("b=y&a=1"));
    assert_eq!(key("a=1&b=y"), Ok((Tier::Exact, 202)));
}

// --- MCK-04 -----------------------------------------------------------------------------

#[test]
fn mck_04_the_answer_is_the_sample_without_framing_headers() {
    let call = ex("GET", "/h", 203)
        .resp(
            "application/json; charset=utf-8",
            r#"{"id":7,"tags":["a","b"],"n":{"x":null}}"#,
        )
        .resp_header("content-length", "999")
        .resp_header("x-a", "1")
        .resp_header("connection", "close")
        .resp_header("keep-alive", "timeout=5")
        .resp_header("te", "trailers")
        .resp_header("trailer", "x-t")
        .resp_header("upgrade", "h2c")
        .resp_header("proxy-connection", "close")
        .resp_header("content-encoding", "gzip")
        .resp_header("transfer-encoding", "chunked")
        .resp_header("x-b", "2");
    let index = index(&record(&[call]));
    let (_, r) = ask(&index, &req("GET", "/h")).expect("an answer");
    assert_eq!(r.status, 203);
    let body = text(&r);
    let value: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        value,
        serde_json::json!({"id": 7, "tags": ["a", "b"], "n": {"x": null}})
    );
    assert!(!body.contains(' '), "compact");
    let expected = [
        (
            "content-type",
            "application/json; charset=utf-8".to_string(),
        ),
        ("x-a", "1".to_string()),
        ("x-b", "2".to_string()),
        ("content-length", body.len().to_string()),
        ("x-mimic-match", "exact".to_string()),
    ];
    let got: Vec<(&str, &str)> = r
        .headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let expected: Vec<(&str, &str)> = expected.iter().map(|(n, v)| (*n, v.as_str())).collect();
    assert_eq!(got, expected);
    assert!(r.trailers.is_empty());
}

#[test]
fn mck_04_a_form_body_encodes_its_pairs_in_order() {
    let call = ex("GET", "/f", 200).resp(FORM, "b=2&a=1&a=3&c=x+y%26z");
    let index = index(&record(&[call]));
    let (_, r) = ask(&index, &req("GET", "/f")).expect("an answer");
    assert_eq!(text(&r), "b=2&a=1&a=3&c=x+y%26z");
    assert_eq!(header(&r, "content-length"), Some("21"));
}

#[test]
fn mck_04_no_body_head_204_and_304_give_an_empty_body() {
    let calls = [
        ex("GET", "/opaque", 200).resp("text/plain", "hello"),
        ex("GET", "/none", 200),
        ex("HEAD", "/h", 200).resp(JSON, r#"{"a":1}"#),
        ex("GET", "/n", 204).resp(JSON, r#"{"a":1}"#),
        ex("GET", "/m", 304).resp(JSON, r#"{"a":1}"#),
    ];
    let index = index(&record(&calls));
    for (method, path) in [
        ("GET", "/opaque"),
        ("GET", "/none"),
        ("HEAD", "/h"),
        ("GET", "/n"),
        ("GET", "/m"),
    ] {
        let (_, r) = ask(&index, &req(method, path)).expect("an answer");
        assert!(r.body().is_empty(), "{method} {path}");
        assert_eq!(header(&r, "content-length"), Some("0"), "{method} {path}");
    }
}

#[test]
fn mck_04_a_grpc_answer_is_ok_with_an_empty_message_and_trailers() {
    let mut call = ex("POST", "/pkg.Svc/Get", 200).resp("application/grpc", "");
    call.grpc = true;
    let index = index(&record(&[call]));
    let mut r = req("POST", "/pkg.Svc/Get").header("content-type", "application/grpc");
    r.h2 = true;
    let (tier, response) = ask(&index, &r).expect("an answer");
    assert_eq!(tier, Tier::Exact);
    assert_eq!(response.status, 200);
    assert_eq!(response.body(), [0u8; 5]);
    assert_eq!(header(&response, "content-type"), Some("application/grpc"));
    assert_eq!(header(&response, "content-length"), Some("5"));
    assert_eq!(
        response.trailers,
        vec![("grpc-status".to_string(), "0".to_string())]
    );
}

#[test]
fn mck_04_a_grpc_answer_drops_the_status_headers_of_its_sample() {
    let mut call = ex("POST", "/pkg.Svc/Get", 200)
        .resp("application/grpc", "")
        .resp_header("grpc-status", "5")
        .resp_header("grpc-message", "not found");
    call.grpc = true;
    let index = index(&record(&[call]));
    let mut r = req("POST", "/pkg.Svc/Get").header("content-type", "application/grpc");
    r.h2 = true;
    let (_, response) = ask(&index, &r).expect("an answer");
    assert!(header(&response, "grpc-status").is_none());
    assert!(header(&response, "grpc-message").is_none());
    assert_eq!(
        response.trailers,
        vec![("grpc-status".to_string(), "0".to_string())]
    );
}

#[test]
fn mck_04_a_miss_is_599_and_nothing_else() {
    for miss in [
        Miss::Authority,
        Miss::Endpoint,
        Miss::Connect,
        Miss::Upgrade,
        Miss::Overload,
    ] {
        let r = miss.response();
        assert_eq!(r.status, 599);
        assert_eq!(
            r.headers,
            vec![
                ("x-mimic-miss".to_string(), "1".to_string()),
                ("content-length".to_string(), "0".to_string())
            ]
        );
        assert!(r.body().is_empty() && r.trailers.is_empty());
    }
}

#[test]
fn mck_04_answers_are_shared_not_copied() {
    let set = record(&[ex("GET", "/s", 200).resp(JSON, r#"{"a":1}"#)]);
    let index = index(&set);
    let r = req("GET", "/s");
    let hit = |i: &Index| r.with(|q| i.lookup(&Redactor::default(), q).map(|h| h.answer));
    let (a, b) = (hit(&index).expect("a hit"), hit(&index).expect("a hit"));
    assert!(std::sync::Arc::ptr_eq(&a, &b));
}

const FIXED: [&str; 2] = ["content-length", "x-mimic-match"];

/// The fixed values of an answer, or a value of one sample of the authority.
fn assert_from_the_set(set: &EgressSet, response: &Response, canary: &str) {
    let samples: Vec<_> = set
        .authorities
        .get(AUTH)
        .map(|a| a.endpoints.iter().flat_map(|e| e.samples.iter()).collect())
        .unwrap_or_default();
    for (name, value) in &response.headers {
        assert!(!value.contains(canary) && !name.contains(canary));
        if FIXED.contains(&name.as_str()) {
            continue;
        }
        let known = samples.iter().any(|s| {
            s.response_headers()
                .iter()
                .any(|(n, v)| n == name && v == value)
        });
        assert!(known, "header {name} comes from no sample");
    }
    let body = response.body();
    assert!(!String::from_utf8_lossy(body).contains(canary));
    let known = body.is_empty()
        || body == [0u8; 5]
        || samples.iter().any(|s| match s.response_payload() {
            Some((_, Payload::Json(t))) => t.as_bytes() == body,
            Some((_, Payload::Form(p))) => {
                let joined: Vec<String> = p.iter().map(|(k, v)| format!("{k}={v}")).collect();
                joined.join("&").as_bytes() == body
            }
            None => false,
        });
    assert!(known, "body comes from no sample");
}

fn arb_ex() -> impl Strategy<Value = Ex> {
    let headers = proptest::sample::subsequence(
        vec![
            ("x-a", "1"),
            ("x-b", "2"),
            ("content-encoding", "gzip"),
            ("trailer", "t"),
        ],
        0..=4,
    );
    let body = prop_oneof![
        Just(None),
        Just(Some((JSON, r#"{"id":1,"v":"s"}"#))),
        Just(Some((FORM, "a=1&b=2"))),
        Just(Some(("text/plain", "opaque"))),
    ];
    let req_body = prop_oneof![
        Just(None),
        Just(Some((JSON, r#"{"k":1,"v":"s"}"#))),
        Just(Some((FORM, "k=1&v=2"))),
    ];
    (
        prop_oneof![Just("GET"), Just("POST"), Just("HEAD")],
        prop_oneof![Just("/a"), Just("/a/1"), Just("/a/2"), Just("/b/x")],
        prop_oneof![Just(""), Just("x=1"), Just("x=2&y=3")],
        prop_oneof![
            Just(200u16),
            Just(201),
            Just(204),
            Just(304),
            Just(404),
            Just(500),
            Just(101)
        ],
        (headers, body, req_body),
    )
        .prop_map(|(method, path, query, status, (headers, body, req_body))| {
            let mut e = ex(method, path, status).query(query);
            for (n, v) in headers {
                e = e.resp_header(n, v);
            }
            if let Some((ct, b)) = body {
                e = e.resp(ct, b);
            }
            if let Some((ct, b)) = req_body {
                e.req_headers.push(("content-type".into(), ct.into()));
                e.req_body = Some(b.as_bytes().to_vec());
            }
            e
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn mck_04_an_answer_comes_from_the_set_and_never_from_the_request(
        exchanges in proptest::collection::vec(arb_ex(), 0..6),
        method in prop_oneof![Just("GET"), Just("POST"), Just("HEAD")],
        path in prop_oneof![Just("/a"), Just("/a/1"), Just("/a/9"), Just("/b/x"), Just("/zz")],
        place in 0u8..4,
        canary in "[A-Za-z0-9]{32}",
    ) {
        let set = record(&exchanges);
        let index = index(&set);
        let mut r = req(method, path);
        match place {
            0 => r = r.header("x-canary", &canary),
            1 => r = r.query(&format!("x={canary}")),
            2 => r = r.json(&format!(r#"{{"k":"{canary}"}}"#)),
            _ => r = r.query(&format!("{canary}=1")),
        }
        if let Ok((_, response)) = ask(&index, &r) {
            assert_from_the_set(&set, &response, &canary);
        }
    }
}

// --- MCK-05 -----------------------------------------------------------------------------

/// The numbers of a draw, in order; each must be below its bound. Bounds are recorded.
struct Script {
    values: VecDeque<u64>,
    bounds: Vec<u64>,
}

impl Script {
    fn new(values: &[u64]) -> Self {
        Script {
            values: values.iter().copied().collect(),
            bounds: Vec::new(),
        }
    }
}

impl Random for Script {
    fn below(&mut self, bound: u64) -> u64 {
        assert!(bound >= 1);
        self.bounds.push(bound);
        let value = self.values.pop_front().unwrap_or(0);
        assert!(value < bound, "{value} is not below {bound}");
        value
    }
}

fn histogram(observations: &[(u64, u64)]) -> Histogram {
    let mut h = Histogram::default();
    for (ms, times) in observations {
        for _ in 0..*times {
            h.observe(*ms);
        }
    }
    h
}

/// The bound the in-bucket draw is given.
fn second_bound(h: &Histogram) -> u64 {
    let mut probe = Script::new(&[0, 0]);
    draw(h, 100, &mut probe);
    *probe.bounds.get(1).expect("an in-bucket draw")
}

#[test]
fn mck_05_a_draw_is_a_whole_number_in_the_chosen_bucket() {
    // Bucket 0 holds 0 and 1.
    let zero = histogram(&[(0, 1)]);
    let seen: std::collections::BTreeSet<u64> = (0..second_bound(&zero))
        .map(|j| draw(&zero, 100, &mut Script::new(&[0, j])))
        .collect();
    assert_eq!(seen, [0, 1].into());
    for ms in [2u64, 3, 5, 10, 100, 1000, 20_000, 131_072] {
        let h = histogram(&[(ms, 1)]);
        let bucket = bucket_index(ms);
        let members: std::collections::BTreeSet<u64> = (0..=131_072)
            .filter(|m| bucket_index(*m) == bucket)
            .collect();
        let seen: std::collections::BTreeSet<u64> = (0..second_bound(&h))
            .map(|j| draw(&h, 100, &mut Script::new(&[0, j])))
            .collect();
        assert_eq!(seen, members, "bucket of {ms} ms");
    }
}

#[test]
fn mck_05_a_bucket_is_chosen_in_proportion_to_its_count() {
    let h = histogram(&[(1, 3), (10, 2), (100, 4), (5000, 1)]);
    let total = h.count();
    assert_eq!(total, 10);
    let mut chosen: SortedMap<u8, u64> = SortedMap::new();
    for r in 0..total {
        let d = draw(&h, 100, &mut Script::new(&[r, 0]));
        *chosen.entry(bucket_index(d)).or_default() += 1;
    }
    assert_eq!(&chosen, h.buckets());
}

#[test]
fn mck_05_the_overflow_bucket_an_empty_histogram_and_the_factor() {
    let overflow = histogram(&[(200_000, 1)]);
    assert_eq!(draw(&overflow, 100, &mut Script::new(&[0, 0])), 131_072);
    assert_eq!(draw(&Histogram::default(), 100, &mut Script::new(&[])), 0);

    let h = histogram(&[(10, 2), (400, 3)]);
    for r in 0..5 {
        for j in 0..2 {
            let at = |f: u32| draw(&h, f, &mut Script::new(&[r, j]));
            assert_eq!(at(0), 0);
            assert_eq!(at(200), 2 * at(100));
            assert!(at(10_000) <= MAX_DELAY_MS);
        }
    }
    // 131 072 ms at 100 times is capped at 300 s.
    assert_eq!(
        draw(&overflow, 10_000, &mut Script::new(&[0, 0])),
        MAX_DELAY_MS
    );
    assert_eq!(draw(&overflow, 200, &mut Script::new(&[0, 0])), 262_144);
    assert_eq!(
        draw(&overflow, 300, &mut Script::new(&[0, 0])),
        MAX_DELAY_MS
    );
}

#[test]
fn mck_05_the_factor_rounds_to_the_nearest_millisecond_halves_up() {
    assert_eq!(scale(3, 50), 2);
    assert_eq!(scale(10, 130), 13);
    assert_eq!(scale(49, 1), 0);
    assert_eq!(scale(50, 1), 1);
    assert_eq!(scale(7, 100), 7);
    assert_eq!(scale(u64::MAX, 10_000), MAX_DELAY_MS);
}

#[test]
fn mck_05_a_factor_reads_in_hundredths_halves_up() {
    for (text, hundredths) in [
        ("0", 0),
        ("0.5", 50),
        ("1", 100),
        ("1.25", 125),
        ("100", 10_000),
        ("100.00", 10_000),
        ("0.005", 1),
        ("0.004", 0),
        ("0.014", 1),
        ("0.015", 2),
        ("100.004", 10_000),
    ] {
        assert_eq!(parse_factor(text), Some(hundredths), "{text}");
    }
    for text in [
        "",
        "-1",
        "+1",
        "101",
        "100.005",
        "abc",
        "1e2",
        ".5",
        "1.",
        " 1",
        "1 ",
        "0x10",
        "NaN",
        "inf",
        "1,5",
        "99999999999999999999999999",
    ] {
        assert_eq!(parse_factor(text), None, "{text:?}");
    }
}

#[test]
fn mck_05_a_factor_is_written_in_the_shortest_form_that_reads_back() {
    for (hundredths, text) in [
        (0, "0"),
        (50, "0.5"),
        (100, "1"),
        (1, "0.01"),
        (120, "1.2"),
        (125, "1.25"),
        (10_000, "100"),
    ] {
        assert_eq!(format_factor(hundredths), text);
    }
    for h in 0..=MAX_FACTOR {
        assert_eq!(parse_factor(&format_factor(h)), Some(h));
    }
}

// --- MCK-06 -----------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn mck_06_a_request_is_matched_as_the_recorder_recorded_it(
        recorded in "[A-Za-z0-9]{32}",
        replayed in "[A-Za-z0-9]{32}",
    ) {
        let call = |canary: &str| {
            ex("POST", "/cards/4111111111111111/charge", 201)
                .query(&format!("api_key={canary}"))
                .req_json(&format!(r#"{{"token":"{canary}","amount":3}}"#))
                .resp(JSON, r#"{"ok":true}"#)
        };
        let mut recorded_call = call(&recorded);
        recorded_call
            .req_headers
            .push(("authorization".into(), format!("Bearer {recorded}")));
        let set = record(&[recorded_call]);
        let index = index(&set);
        for canary in [&recorded, &replayed] {
            let r = req("POST", "/cards/4111111111111111/charge")
                .query(&format!("api_key={canary}"))
                .json(&format!(r#"{{"token":"{canary}","amount":3}}"#))
                .header("authorization", &format!("Bearer {canary}"));
            let (tier, response) = ask(&index, &r).expect("an answer");
            prop_assert_eq!(tier, Tier::Exact);
            prop_assert_eq!(response.status, 201);
            let signature = r.with(|q| signature(&Redactor::default(), q));
            prop_assert_eq!(signature.authority(), AUTH);
            let shown = format!("{signature:?}");
            let clean = signature.fields().iter().all(|(name, values)| {
                !name.contains(canary.as_str()) && values.iter().all(|v| !v.contains(canary.as_str()))
            });
            prop_assert!(!shown.contains(canary.as_str()));
            prop_assert!(!signature.path().contains("4111"));
            prop_assert!(clean);
        }
    }
}

#[test]
fn mck_06_mode_headers_are_read_exactly() {
    assert_eq!(parse_mode_header(&["discovery"]), Some(Mode::Discovery));
    assert_eq!(parse_mode_header(&["mock"]), Some(Mode::Mock));
    for values in [
        &["MOCK"][..],
        &["Discovery"],
        &[""],
        &[" mock"],
        &["mock", "mock"],
        &["mock", "discovery"],
        &["record"],
        &[],
    ] {
        assert_eq!(parse_mode_header(values), None, "{values:?}");
    }
    assert_eq!(parse_factor_header(&["0.5"]), Some(50));
    assert_eq!(parse_factor_header(&["100"]), Some(10_000));
    for values in [&["0.5", "1"][..], &[""], &["101"], &["-1"], &["x"], &[]] {
        assert_eq!(parse_factor_header(values), None, "{values:?}");
    }
    for factor in ["1", "0.5", "0", "100"] {
        let hundredths = parse_factor_header(&[factor]).expect("a factor");
        assert_eq!(format_factor(hundredths), factor);
    }
}

// --- MCK-01: the effective mode ----------------------------------------------------------

fn settings(on: bool, factor: u32, passthrough: &[&str]) -> MockSettings {
    MockSettings {
        mock: on,
        factor,
        passthrough: passthrough.iter().map(|p| p.to_string()).collect(),
    }
}

#[test]
fn mck_01_no_workload_gives_discovery_factor_1_no_passthrough() {
    let none = effective([]);
    assert_eq!(
        (none.mode(), none.factor_text()),
        (Mode::Discovery, "1".into())
    );
    assert!(none.passthrough.is_empty());
    let discovery = settings(false, 100, &[]);
    assert_eq!(effective([&discovery]).mode(), Mode::Discovery);
    assert_eq!(effective([&MockSettings::default()]), none);
}

#[test]
fn mck_01_one_mock_workload_wins_and_the_first_in_order_gives_the_factor() {
    let (a, b, c) = (
        settings(false, 700, &[]),
        settings(true, 50, &[]),
        settings(true, 300, &[]),
    );
    let found = effective([&a, &b, &c]);
    assert_eq!(
        (found.mode(), found.factor_text()),
        (Mode::Mock, "0.5".into())
    );
    assert_eq!(effective([&a, &c]).factor, 300);
    assert_eq!(effective([&a]).mode(), Mode::Discovery);
}

#[test]
fn mck_01_passthrough_is_the_sorted_deduplicated_union_cut_to_256() {
    let (a, b) = (
        settings(false, 100, &["b.test", "a.test"]),
        settings(true, 100, &["b.test", "*.c.test"]),
    );
    assert_eq!(
        effective([&a, &b]).passthrough,
        ["*.c.test", "a.test", "b.test"]
    );
    let many: Vec<String> = (0..200).map(|i| format!("h{i:03}.test")).collect();
    let more: Vec<String> = (100..300).map(|i| format!("h{i:03}.test")).collect();
    fn refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    let (a, b) = (
        settings(false, 100, &refs(&many)),
        settings(false, 100, &refs(&more)),
    );
    let union = effective([&a, &b]).passthrough;
    assert_eq!(union.len(), 256);
    assert_eq!(
        (union[0].as_str(), union[255].as_str()),
        ("h000.test", "h255.test")
    );
}

#[test]
fn mck_01_factor_text_is_the_shortest_decimal() {
    let texts: Vec<String> = [100, 50, 0, 10_000, 5, 130, 101]
        .into_iter()
        .map(|f| effective([&settings(true, f, &[])]).factor_text())
        .collect();
    assert_eq!(texts, ["1", "0.5", "0", "100", "0.05", "1.3", "1.01"]);
}
