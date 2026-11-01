//! F4 follow-ups (issue #280): token-boundary names, percent-decoded detectors, relative URLs,
//! merge limits and saturation, volatility counted once per exchange.

use serde_json::json;

use super::observed::Observation;
use super::redact::{Redactor, Rule, path, segment_rule};
use super::report::{Direction, Exchange, Limits, Report, merge};
use super::schema::kept_headers;
use super::template::is_param;

const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";
/// `eyJhbGciOiJIUzI1NiJ9` with its first letter percent-encoded.
const JWT_ENCODED: &str = "%65yJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln";

#[test]
fn dsc_05_names_match_on_token_boundaries() {
    let r = Redactor::new(["x-tenant-key", "pin"]);
    for n in [
        "x-api-key",
        "X-Api-Key",
        "auth_token",
        "Authorization",
        "auth.token",
        "authToken",
        "AuthToken",
        "APIKey",
        "api_key",
        "client_secret",
        "refreshToken",
        "Session-Id",
        "tokens",
        "passwords",
        "token2",
        "card.number",
        "cardNumber",
        "X_TENANT-KEY",
        "xTenantKey",
        "pin_code",
        "PIN",
        "set-cookie",
        "private_key",
    ] {
        assert!(r.name_matches(n), "{n} should be redacted");
    }
    for n in [
        "JSESSIONID",
        "sessionid",
        "csrftoken",
        "accesstoken",
        "authtoken",
        "XAPIKEY",
        "xapikey",
        "clientsecret",
        "SECRETKEY",
        "userpassword",
        "dbpassword",
    ] {
        assert!(r.name_matches(n), "{n} should be redacted");
    }
    for n in [
        "company",
        "author",
        "panel",
        "passenger",
        "company_name",
        "authors",
        "expand",
        "pinned",
        "id",
        "name",
        "amount",
    ] {
        assert!(!r.name_matches(n), "{n} should be kept");
    }
    // The same rule for the extra names: a substring is not a token.
    let r = Redactor::new(["badge"]);
    assert!(r.name_matches("X-Badge"));
    assert!(!r.name_matches("badgers"));
}

#[test]
fn dsc_06_percent_encoded_path_segments_are_detected() {
    assert_eq!(segment_rule(JWT_ENCODED), Some(Rule::Jwt));
    assert_eq!(segment_rule("Bearer%20abc"), Some(Rule::Credential));
    assert_eq!(segment_rule("%41"), None);
    assert_eq!(
        path(&format!("/reset/{JWT_ENCODED}/done")),
        "/reset/<redacted>/done"
    );
    // A kept segment stays as received.
    assert_eq!(path("/a%20b/%41"), "/a%20b/%41");
    assert!(is_param(JWT_ENCODED));
    assert!(!is_param("%41"));
}

#[test]
fn dsc_06_percent_encoded_query_values_in_header_urls() {
    let r = Redactor::default();
    assert_eq!(
        r.header("Location", &format!("https://h/x?v={JWT_ENCODED}&k=a%20b")),
        "https://h/x?v=<redacted>&k=a%20b"
    );
}

#[test]
fn dsc_05_relative_urls_in_header_values() {
    let r = Redactor::default();
    assert_eq!(
        r.header("Location", "reset?code=SplxlOBeZQQ&access_token=abc"),
        "reset?code=SplxlOBeZQQ&access_token=<redacted>"
    );
    assert_eq!(
        r.header("Link", "<cb?token=T>; rel=next"),
        "<cb?token=<redacted>>; rel=next"
    );
    assert_eq!(
        r.header("Location", &format!("../reset/{JWT}")),
        "../reset/<redacted>"
    );
    assert_eq!(
        r.header("Location", "cb#access_token=T"),
        "cb#access_token=<redacted>"
    );
    assert_eq!(
        r.header("Content-Type", "text/html; charset=utf-8"),
        "text/html; charset=utf-8"
    );
}

#[test]
fn dsc_05_header_rules_count_url_redactions() {
    let r = Redactor::default();
    assert_eq!(r.header_rules("x-api-key", "k"), [Rule::Name]);
    assert_eq!(r.header_rules("x-custom", JWT), [Rule::Jwt]);
    assert_eq!(
        r.header_rules("location", &format!("https://h/reset/{JWT}?token=T&page=2")),
        [Rule::Jwt, Rule::Name]
    );
    assert_eq!(r.header_rules("location", "/home?page=2"), []);
    assert_eq!(
        r.header_rules("link", "<cb?token=T>; rel=next"),
        [Rule::Name]
    );
}

#[test]
fn dsc_05_only_the_64_kept_headers_are_counted() {
    let names: Vec<String> = (0..100).map(|i| format!("h{i:03}")).collect();
    let headers: Vec<(&str, &str)> = names
        .iter()
        .map(|n| (n.as_str(), "v"))
        .chain([("Connection", "close"), (":path", "/")])
        .collect();
    let kept = kept_headers(&headers);
    assert_eq!(kept.len(), 64);
    assert_eq!(kept[0].0, "h000");
    assert_eq!(kept[63].0, "h063");
}

fn exchange<'a>(path: &'a str, observation: Observation<'a>) -> Exchange<'a> {
    Exchange {
        direction: Direction::Inbound,
        port: 80,
        method: "GET",
        path,
        status: 200,
        head_ms: 1,
        host: None,
        grpc: false,
        observation,
    }
}

fn limits(max_endpoints: usize, sample_budget: usize) -> Limits {
    Limits {
        max_literals: 32,
        max_endpoints,
        sample_budget,
    }
}

#[test]
fn dsc_04_n_counts_an_exchange_once() {
    let body =
        json!({"items": (0..10).map(|i| json!({"id": format!("id-{i}")})).collect::<Vec<_>>()});
    let body = body.to_string();
    let headers = [("content-type", "application/json")];
    let mut report = Report::new(None);
    let l = limits(10, 1 << 20);
    for calls in 1..=3 {
        report.record(
            &l,
            &Redactor::default(),
            &exchange(
                "/x",
                Observation {
                    resp_headers: &headers,
                    resp_body: Some(body.as_bytes()),
                    ..Observation::default()
                },
            ),
        );
        let json = report.endpoints()[0].stats.observed().to_json();
        let field = &json["fields"]["response.body/application/json/items/*/id"];
        assert_eq!(field["n"], calls, "{field}");
    }
    // Ten distinct values per call and three calls: volatile only from the third call.
    let json = report.endpoints()[0].stats.observed().to_json();
    assert_eq!(
        json["fields"]["response.body/application/json/items/*/id"]["volatile"],
        true
    );
}

fn report_with(paths: &[&str], status: u16, l: &Limits) -> Report {
    let mut r = Report::new(None);
    let body = json!({"k": "v"}).to_string();
    let headers = [("content-type", "application/json")];
    for p in paths {
        let mut e = exchange(
            p,
            Observation {
                resp_headers: &headers,
                resp_body: Some(body.as_bytes()),
                ..Observation::default()
            },
        );
        e.status = status;
        r.record(l, &Redactor::default(), &e);
    }
    r
}

#[test]
fn dsc_09_counters_saturate() {
    let l = limits(10, 1 << 20);
    let r = report_with(&["/a"], 200, &l);
    let json = r.to_json().unwrap();
    let max = json.replace("\"200\":1", &format!("\"200\":{}", u64::MAX));
    assert_ne!(json, max);
    let big = Report::from_json(&max).unwrap();
    let merged = merge(big.clone(), big, &l);
    assert_eq!(
        merged.endpoints()[0].stats.statuses.get(&200),
        Some(&u64::MAX)
    );
    assert_eq!(merged.exchanges(), 2);
}

#[test]
fn dsc_09_merge_keeps_the_first_endpoints_past_the_limit() {
    let l = limits(4, 1 << 20);
    let a = report_with(&["/a", "/c", "/e"], 200, &l);
    let b = report_with(&["/b", "/d", "/f"], 200, &l);
    let ab = merge(a.clone(), b.clone(), &l);
    let ba = merge(b, a, &l);
    assert_eq!(ab, ba);
    let templates: Vec<_> = ab.endpoints().into_iter().map(|e| e.template).collect();
    assert_eq!(templates, ["/a", "/b", "/c", "/d"]);
    assert_eq!(Report::from_json(&ab.to_json().unwrap()).unwrap(), ab);
}

#[test]
fn dsc_09_merge_keeps_the_samples_within_the_budget() {
    let big = limits(10, 1 << 20);
    let a = report_with(&["/a", "/b"], 200, &big);
    let b = report_with(&["/c", "/d"], 201, &big);
    let one = a.sample_bytes() / 2;
    let tight = limits(10, 3 * one);
    let ab = merge(a.clone(), b.clone(), &tight);
    let ba = merge(b, a, &tight);
    assert_eq!(ab, ba);
    assert!(ab.sample_bytes() <= 3 * one, "{}", ab.sample_bytes());
    assert!(ab.sample_bytes() > 0);
    // Counters are untouched by the budget.
    assert_eq!(ab.endpoints().len(), 4);
}
