//! Critères d'acceptation RED-* de la spec (§7 et §11.2) : redaction.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::model::{Body, Exchange, Header};
use mimic_core::redact::{RedactConfig, Redactor};
use proptest::prelude::*;
use serde_json::{Value, json};

fn redactor() -> Redactor {
    Redactor::new(RedactConfig::default(), b"test-salt".to_vec())
}

fn redacted(mut e: Exchange) -> Exchange {
    redactor().redact(&mut e);
    e
}

fn resp_json(e: &Exchange) -> Value {
    serde_json::from_slice(e.resp_body.bytes().unwrap()).unwrap()
}

fn req_json(e: &Exchange) -> Value {
    serde_json::from_slice(e.req_body.bytes().unwrap()).unwrap()
}

fn is_marker(v: &str) -> bool {
    v.len() == "<redacted:sha256:00000000>".len()
        && v.starts_with("<redacted:sha256:")
        && v.ends_with('>')
}

#[test]
fn red_01_authorization_header_is_replaced_by_marker() {
    let e = redacted(exchange("GET", "/").with_req_header("Authorization", "Bearer abc"));
    let v = e.req_header("authorization").unwrap();
    assert!(is_marker(v), "{v}");
    assert!(!serde_json::to_string(&e).unwrap().contains("abc"));
    assert!(
        e.redactions
            .iter()
            .any(|m| m.location == "request.header.authorization")
    );
}

#[test]
fn red_01b_sensitive_header_patterns() {
    let e = redacted(
        exchange("GET", "/")
            .with_req_header("Cookie", "sid=1")
            .with_req_header("X-Api-Key", "k")
            .with_req_header("X-Session-Id", "s")
            .with_req_header("Accept", "application/json")
            .with_resp_header("Set-Cookie", "sid=2"),
    );
    assert!(is_marker(e.req_header("cookie").unwrap()));
    assert!(is_marker(e.req_header("x-api-key").unwrap()));
    assert!(is_marker(e.req_header("x-session-id").unwrap()));
    assert_eq!(e.req_header("accept").unwrap(), "application/json");
    assert!(is_marker(e.resp_header("set-cookie").unwrap()));
}

#[test]
fn red_02_same_secret_gives_same_marker_and_salt_matters() {
    let a = redacted(exchange("GET", "/").with_req_header("Authorization", "Bearer abc"));
    let b = redacted(exchange("GET", "/x").with_req_header("Authorization", "Bearer abc"));
    let c = redacted(exchange("GET", "/").with_req_header("Authorization", "Bearer xyz"));
    assert_eq!(a.req_header("authorization"), b.req_header("authorization"));
    assert_ne!(a.req_header("authorization"), c.req_header("authorization"));

    let mut d = exchange("GET", "/").with_req_header("Authorization", "Bearer abc");
    Redactor::new(RedactConfig::default(), b"other-salt".to_vec()).redact(&mut d);
    assert_ne!(a.req_header("authorization"), d.req_header("authorization"));
}

#[test]
fn red_03_card_numbers_are_detected_with_luhn() {
    let e = redacted(exchange("POST", "/pay").req_json(json!({
        "card": {"number": "4111 1111 1111 1111"},
        "note": "4111 1111 1111 1112",
    })));
    let body = req_json(&e);
    assert!(is_marker(body["card"]["number"].as_str().unwrap()));
    assert_eq!(body["note"], "4111 1111 1111 1112");
}

#[test]
fn red_03b_emails_jwts_and_ibans_are_detected_in_values() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
    let e = redacted(exchange("GET", "/u").resp_json(json!({
        "contact": "jane@example.com",
        "t": jwt,
        "bank": "FR7630006000011234567890189",
        "name": "Jane",
        "list": ["bob@example.com", "ok"],
    })));
    let body = resp_json(&e);
    assert!(is_marker(body["contact"].as_str().unwrap()));
    assert!(is_marker(body["t"].as_str().unwrap()));
    assert!(is_marker(body["bank"].as_str().unwrap()));
    assert_eq!(body["name"], "Jane");
    assert!(is_marker(body["list"][0].as_str().unwrap()));
    assert_eq!(body["list"][1], "ok");
    assert!(
        e.redactions
            .iter()
            .any(|m| m.location == "response.body$.contact")
    );
}

#[test]
fn sensitive_keys_redact_whole_value_whatever_its_type() {
    let e = redacted(exchange("POST", "/login").req_json(json!({
        "user": "jane",
        "password": "hunter2",
        "credentials": {"api_key": 12345, "refresh_token": ["a", "b"]},
    })));
    let body = req_json(&e);
    assert_eq!(body["user"], "jane");
    assert!(is_marker(body["password"].as_str().unwrap()));
    assert!(is_marker(body["credentials"]["api_key"].as_str().unwrap()));
    assert!(is_marker(
        body["credentials"]["refresh_token"].as_str().unwrap()
    ));
}

#[test]
fn extra_json_paths_are_redacted() {
    let config = RedactConfig {
        extra_json_paths: vec!["$.customer.phone".into()],
        ..RedactConfig::default()
    };
    let mut e = exchange("GET", "/c")
        .resp_json(json!({"customer": {"phone": "0600000000", "city": "Lyon"}}));
    Redactor::new(config, b"s".to_vec()).redact(&mut e);
    let body = resp_json(&e);
    assert!(is_marker(body["customer"]["phone"].as_str().unwrap()));
    assert_eq!(body["customer"]["city"], "Lyon");
}

#[test]
fn query_params_with_sensitive_names_are_redacted() {
    let e = redacted(exchange("GET", "/cb?access_token=abc&page=2"));
    assert!(is_marker(&e.query[0].1));
    assert_eq!(e.query[1], ("page".into(), "2".into()));
}

#[test]
fn form_urlencoded_bodies_are_redacted() {
    let mut e = exchange("POST", "/v1/charges")
        .with_req_header("Content-Type", "application/x-www-form-urlencoded");
    e.req_body =
        Body::captured("amount=100&card%5Bnumber%5D=4242424242424242&client_secret=s3cr3t");
    let e = redacted(e);
    let body = String::from_utf8(e.req_body.bytes().unwrap().to_vec()).unwrap();
    assert!(body.starts_with("amount=100&"), "{body}");
    assert!(!body.contains("4242424242424242"), "{body}");
    assert!(!body.contains("s3cr3t"), "{body}");
}

#[test]
fn truncated_and_unparseable_bodies_are_dropped_when_they_may_leak() {
    let mut e = exchange("POST", "/x").with_req_header("Content-Type", "application/json");
    e.req_body = Body::Truncated {
        size: 10_000,
        prefix: br#"{"password":"hunter2""#.to_vec(),
    };
    e.resp_headers
        .push(Header::new("content-type", "application/json"));
    e.resp_body = Body::captured(r#"{"password": "hunter2""#);
    let e = redacted(e);
    assert_eq!(e.req_body, Body::Skipped { size: 10_000 });
    assert_eq!(e.resp_body, Body::Skipped { size: 22 });
}

#[test]
fn binary_bodies_are_left_untouched() {
    let mut e = exchange("GET", "/img").with_resp_header("Content-Type", "image/png");
    e.resp_body = Body::captured(vec![0x89, b'P', b'N', b'G']);
    let before = e.resp_body.clone();
    let e = redacted(e);
    assert_eq!(e.resp_body, before);
}

#[test]
fn redaction_is_idempotent() {
    let once = redacted(
        exchange("POST", "/login")
            .with_req_header("Authorization", "Bearer abc")
            .req_json(json!({"password": "p", "mail": "a@b.io"})),
    );
    let twice = redacted(once.clone());
    assert_eq!(once.req_headers, twice.req_headers);
    assert_eq!(once.req_body, twice.req_body);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// RED-04 : un secret injecté à un emplacement sensible n'apparaît dans aucun octet stocké.
    #[test]
    fn red_04_secret_never_reaches_the_store(
        secret in "S3CR3T[A-Za-z0-9]{16}",
        depth in 0usize..4,
        key in prop::sample::select(vec!["password", "token", "apiKey", "client_secret", "sessionId"]),
        place in 0usize..5,
    ) {
        let mut nested = json!({ key: secret.clone() });
        for i in 0..depth {
            nested = json!({ format!("level{i}"): nested, "other": i });
        }
        let e = match place {
            0 => exchange("GET", "/").with_req_header("Authorization", &format!("Bearer {secret}")),
            1 => exchange("GET", "/").with_req_header("Cookie", &format!("sid={secret}")),
            2 => exchange("GET", &format!("/?{key}={secret}")),
            3 => exchange("POST", "/").req_json(nested),
            _ => exchange("GET", "/").resp_json(nested),
        };
        let stored = serde_json::to_string(&redacted(e)).unwrap();
        // Le body est encodé en base64 dans le store : on vérifie aussi la forme décodée.
        let decoded: Exchange = serde_json::from_str(&stored).unwrap();
        let bodies = [decoded.req_body.bytes(), decoded.resp_body.bytes()];
        prop_assert!(!stored.contains(&secret));
        for b in bodies.into_iter().flatten() {
            prop_assert!(!String::from_utf8_lossy(b).contains(&secret));
        }
    }
}
