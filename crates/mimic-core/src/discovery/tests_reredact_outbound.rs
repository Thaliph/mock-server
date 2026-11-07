//! SHR-02: re-redaction with the outbound view (spec §7.3), plus the follow-ups of #322 and
//! #323. A faulty or F7-era sidecar is simulated by recording the exchange as inbound (no
//! outbound view), moving the scope under an authority and swapping placeholders for the
//! canaries in the JSON text. Canaries never appear in assertion messages.

use std::net::SocketAddr;

use proptest::prelude::*;
use serde_json::{Value, json};
use xxhash_rust::xxh3::xxh3_64;

use super::observed::Observation;
use super::redact::{MARKER, Redactor};
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

/// A JWT whose body part carries `canary`.
fn jwt(canary: &str) -> String {
    format!("eyJhbGciOiJIUzI1NiJ9.{canary}.c2ln")
}

/// The detected forms that go into an allowlisted header: the value and the secret part of it.
pub(super) fn form(i: usize, canary: &str) -> (String, String) {
    match i % 4 {
        0 => (jwt(canary), canary.to_string()),
        1 => (format!("AIza{canary}"), canary.to_string()),
        2 => (format!("ghp_{canary}"), canary.to_string()),
        _ => (
            "4111111111111111".to_string(),
            "4111111111111111".to_string(),
        ),
    }
}

/// A report that records `observation` on `POST /x` as a pod without the outbound view would,
/// moved under `authority` with the given origin SNIs, as the JSON text.
fn leaked_json(observation: Observation<'_>, authority: &str, sni: &[&str]) -> String {
    let mut r = Report::new(None);
    r.record(
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port: 8080,
            method: "POST",
            path: "/x",
            status: 200,
            head_ms: 1,
            host: None,
            grpc: false,
            observation,
        },
    );
    let mut v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    let mut scope = v["scopes"]["inbound"]["8080"].take();
    scope["origin"] = json!({"schemes": {"https": 1}, "sni": sni, "dst": ["10.0.0.5:443"]});
    v["scopes"] = json!({ "outbound": { authority: scope } });
    v["version"] = json!(2);
    v.to_string()
}

pub(super) fn hash_free(text: &str, needle: &str) -> bool {
    let hash = xxh3_64(needle.as_bytes());
    !text.contains(needle)
        && !text.contains(&hash.to_string())
        && !text.contains(&format!("{hash:x}"))
}

fn fields(v: &Value, authority: &str) -> Value {
    v["scopes"]["outbound"][authority]["root"]["literals"]["x"]["endpoints"]["POST"]["observed"]
        ["fields"]["fields"]
        .clone()
}

/// Request and response of the leaked exchange, one placeholder per place.
pub(super) struct Places<'a> {
    headers: Vec<(&'a str, &'a str)>,
    query: &'a str,
    body: &'a [u8],
    resp_headers: Vec<(&'a str, &'a str)>,
    resp_body: &'a [u8],
}

pub(super) fn leak_all(p: &Places<'_>, authority: &str, sni: &[&str]) -> String {
    let mut headers = p.headers.clone();
    headers.push(("content-type", "application/json"));
    let mut resp = p.resp_headers.clone();
    resp.push(("content-type", "application/json"));
    leaked_json(
        Observation {
            query: p.query,
            req_headers: &headers,
            req_body: Some(p.body),
            resp_headers: &resp,
            resp_body: Some(p.resp_body),
        },
        authority,
        sni,
    )
}

const BODY: &[u8] = br#"{"zzp3zz":"zzp3azz","a":{"b":{"c":{"d":{"znm":"zzp4zz"}}}}}"#;
const RESP_BODY: &[u8] = br#"{"znr":"zzp8zz","ok":true}"#;

pub(super) fn standard() -> Places<'static> {
    Places {
        headers: vec![
            ("x-extra", "zzp1zz"),
            ("x-zzh1zz", "zzp5zz"),
            ("user-agent", "zzp6zz"),
            ("if-none-match", "zzp7zz"),
        ],
        query: "key=zzp2zz&sig=zzp2azz&ok=fine",
        body: BODY,
        resp_headers: vec![("x-zzh2zz", "zzp9zz")],
        resp_body: RESP_BODY,
    }
}

/// The leaked JSON with every placeholder swapped for its canary form.
pub(super) fn plant(text: &str, canary: &str, ua: usize, etag: usize) -> String {
    text.replace("zzp1zz", canary)
        .replace("zzp2zz", canary)
        .replace("zzp2azz", canary)
        .replace("zzp3azz", canary)
        .replace("zzp4zz", canary)
        .replace("zzp5zz", canary)
        .replace("x-zzh1zz", "password")
        .replace("zzp6zz", &form(ua, canary).0)
        .replace("zzp7zz", &form(etag, canary).0)
        .replace("zzp8zz", canary)
        .replace("zzp9zz", canary)
        .replace("x-zzh2zz", "x-api-key")
        .replace("\"zzp3zz\"", "\"idempotency_key\"")
        .replace("\"znm\"", "\"password\"")
        .replace("\"znr\"", "\"access_token\"")
}

fn assert_clean(r: &Report, canary: &str, ua: usize, etag: usize) -> Result<(), TestCaseError> {
    let text = r.to_json().unwrap();
    for needle in [canary.to_string(), form(ua, canary).1, form(etag, canary).1] {
        prop_assert!(hash_free(&text, &needle));
    }
    Ok(())
}

#[test]
fn shr_02_a_detected_form_in_an_allowlisted_header_is_redacted_by_value() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    for ua in 0..4 {
        let planted = plant(&leak_all(&standard(), AUTHORITY, &[]), canary, ua, ua + 1);
        let mut r = Report::from_json(&planted).unwrap();
        assert!(
            r.to_json().unwrap().contains(&form(ua, canary).1),
            "planted"
        );
        r.re_redact(&Redactor::default());
        let text = r.to_json().unwrap();
        for needle in [
            form(ua, canary).1,
            form(ua + 1, canary).1,
            canary.to_string(),
        ] {
            assert!(hash_free(&text, &needle), "form {ua}");
        }
        let (_, s) = r.endpoints()[0].stats.samples().iter().next().unwrap();
        let ua_value = &s
            .request_headers()
            .iter()
            .find(|(n, _)| n == "user-agent")
            .unwrap()
            .1;
        assert_eq!(ua_value, MARKER, "form {ua}");
    }
}

#[test]
fn shr_02_listed_names_as_header_name_and_deep_innermost_json_key() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let planted = plant(&leak_all(&standard(), AUTHORITY, &[]), canary, 0, 1);
    assert!(planted.contains("\"password\""));
    let mut r = Report::from_json(&planted).unwrap();
    r.re_redact(&Redactor::default());
    let text = r.to_json().unwrap();
    assert!(hash_free(&text, canary));
    let v: Value = serde_json::from_str(&text).unwrap();
    let flags = fields(&v, AUTHORITY);
    assert_eq!(flags["request.header/password"]["redacted"], true);
    assert_eq!(flags["request.header/x-extra"]["redacted"], true);
    assert_eq!(flags["response.header/x-api-key"]["redacted"], true);
}

#[test]
fn shr_02_response_side_canaries_are_redacted() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let planted = plant(&leak_all(&standard(), AUTHORITY, &[]), canary, 0, 1);
    let mut r = Report::from_json(&planted).unwrap();
    r.re_redact(&Redactor::default());
    let (_, s) = r.endpoints()[0].stats.samples().iter().next().unwrap();
    let api_key = s
        .response_headers()
        .iter()
        .find(|(n, _)| n == "x-api-key")
        .unwrap();
    assert_eq!(api_key.1, MARKER);
    assert!(hash_free(&r.to_json().unwrap(), canary));
}

#[test]
fn shr_02_outbound_response_and_inbound_request_keep_their_values() {
    let headers = [("x-anything", "v")];
    let text = leaked_json(
        Observation {
            query: "key=v",
            req_headers: &headers,
            resp_headers: &headers,
            ..Observation::default()
        },
        AUTHORITY,
        &[],
    );
    let mut r = Report::from_json(&text).unwrap();
    r.re_redact(&Redactor::default());
    let (_, s) = r.endpoints()[0].stats.samples().iter().next().unwrap();
    assert_eq!(s.request_headers()[0].1, MARKER, "outbound request");
    assert_eq!(s.query()[0].1, MARKER, "outbound request ?key=");
    assert_eq!(s.response_headers()[0].1, "v", "outbound response");
}

#[test]
fn shr_02_aiza_inside_a_longer_string() {
    use super::redact::value_rule;
    let key = format!("AIza{}", "Xy-_0".repeat(7));
    // After a credential scheme it is part of a credential, caught as one.
    assert!(value_rule(&format!("Bearer {key}")).is_some());
    assert!(value_rule(&format!("Basic {key}")).is_some());
    // As the whole value of a header, a query or a JSON string it is the key itself.
    assert!(value_rule(&key).is_some());
    // Embedded in free text the detector does not look: the documented limit (spec §11.1).
    assert!(value_rule(&format!("note {key}")).is_none());
    assert!(value_rule(&format!("{key}x")).is_none());
    // In an allowlisted outbound header, re-redaction catches the whole-value forms.
    let planted = leak_all(&standard(), AUTHORITY, &[])
        .replace("zzp6zz", &format!("Bearer {key}"))
        .replace("zzp7zz", &key);
    let mut r = Report::from_json(&planted).unwrap();
    r.re_redact(&Redactor::default());
    let text = r.to_json().unwrap();
    assert!(!text.contains(&key));
}

#[test]
fn shr_02_userinfo_in_an_h2_authority_never_reaches_the_report() {
    let d: SocketAddr = "10.0.0.5:443".parse().unwrap();
    let password = "pwcnry8f3a1b7d9e2c4a6f0b5d7e";
    for (case, target) in [
        format!("user:{password}@api.example.com:443"),
        format!("user:{password}@api.example.com"),
        format!("{password}@api.example.com:443"),
    ]
    .iter()
    .enumerate()
    {
        let key = authority_key(Some(target), 443, None, d);
        // Never the target or the key in a message: both may hold the canary.
        assert!(!key.contains(password), "case {case}");
        assert!(!key.contains('@'), "case {case}");
        assert!(ScopeKey::parse(&key).is_some(), "case {case}");
    }
    let mut r = Report::new(None);
    let target = format!("user:{password}@api.example.com:443");
    let key = authority_key(Some(&target), 443, None, d);
    r.record_at(
        ScopeKey::Authority(key),
        &Via {
            tls: true,
            sni: Some("api.example.com"),
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
            host: Some(&target),
            grpc: false,
            observation: Observation::default(),
        },
    );
    r.re_redact(&Redactor::default());
    assert!(!r.to_json().unwrap().contains(password));
}

/// A leaked report with endpoints in `authorities`, each with a tunnel count.
fn two_scopes(a: &str, b: &str) -> Report {
    let one = |authority: &str, path: &str| {
        let mut r = Report::new(None);
        r.record_at(
            ScopeKey::Authority(authority.into()),
            &Via {
                tls: false,
                sni: None,
                dst: None,
            },
            &limits(),
            &Redactor::default(),
            &Exchange {
                direction: Direction::Outbound,
                port: 0,
                method: "GET",
                path,
                status: 200,
                head_ms: 1,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
        r.record_tunnel(&limits(), ScopeKey::Authority(authority.into()), "tcp");
        r
    };
    let mut r = one(a, "/x");
    r.merge(&limits(), one(b, "/y"));
    r
}

#[test]
fn shr_02_a_caught_authority_host_becomes_redacted_and_merges() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let host = format!("sk-{canary}.example.com:443");
    let leaked = two_scopes(&format!("{MARKER}:443"), "zzhostzz.example.com:443")
        .to_json()
        .unwrap()
        .replace("zzhostzz.example.com", &format!("sk-{canary}.example.com"));
    let mut r = Report::from_json(&leaked).unwrap();
    assert!(r.to_json().unwrap().contains(&host), "planted");
    r.re_redact(&Redactor::default());
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    let scopes = v["scopes"]["outbound"].as_object().unwrap();
    assert_eq!(
        scopes.keys().collect::<Vec<_>>(),
        [&format!("{MARKER}:443")]
    );
    let merged = &scopes[&format!("{MARKER}:443")];
    assert_eq!(merged["tunnels"]["tcp"], 2);
    assert!(merged["root"]["literals"]["x"].is_object());
    assert!(merged["root"]["literals"]["y"].is_object());
    assert!(hash_free(&r.to_json().unwrap(), canary));
    assert_eq!(r.version(), 2);
    assert_eq!(r.check(256, 64), Ok(()));
    // Another port stays its own scope; a plain host is untouched.
    let leaked = two_scopes("ok.example.com:443", "zzhostzz.example.com:80")
        .to_json()
        .unwrap()
        .replace("zzhostzz.example.com", &format!("sk-{canary}.example.com"));
    let mut r = Report::from_json(&leaked).unwrap();
    r.re_redact(&Redactor::default());
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    let keys: Vec<&String> = v["scopes"]["outbound"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(
        keys,
        [&format!("{MARKER}:80"), &"ok.example.com:443".to_string()]
    );
}

#[test]
fn shr_02_a_caught_authority_host_alone_is_renamed() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let leaked = two_scopes("zzhostzz.example.com:443", "zzhostzz.example.com:443")
        .to_json()
        .unwrap()
        .replace("zzhostzz.example.com", &format!("ghp_{canary}.example.com"));
    let mut r = Report::from_json(&leaked).unwrap();
    r.re_redact(&Redactor::default());
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    assert!(v["scopes"]["outbound"][format!("{MARKER}:443")].is_object());
    assert!(hash_free(&r.to_json().unwrap(), canary));
}

#[test]
fn shr_02_a_caught_sni_is_removed() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let bad = format!("sk-{canary}.example.com");
    let text = leaked_json(
        Observation::default(),
        AUTHORITY,
        &[&bad, "api.example.com"],
    );
    let mut r = Report::from_json(&text).unwrap();
    assert!(r.to_json().unwrap().contains(&bad), "planted");
    r.re_redact(&Redactor::default());
    let v: Value = serde_json::from_str(&r.to_json().unwrap()).unwrap();
    assert_eq!(
        v["scopes"]["outbound"][AUTHORITY]["origin"]["sni"],
        json!(["api.example.com"])
    );
    assert_eq!(
        v["scopes"]["outbound"][AUTHORITY]["origin"]["dst"],
        json!(["10.0.0.5:443"])
    );
    assert!(hash_free(&r.to_json().unwrap(), canary));
}

#[test]
fn shr_02_idempotent_and_unchanged_for_a_report_recorded_with_the_same_rules() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let planted = plant(
        &leak_all(&standard(), AUTHORITY, &["a.example.com"]),
        canary,
        0,
        1,
    );
    let mut once = Report::from_json(&planted).unwrap();
    once.re_redact(&Redactor::default());
    let mut twice = once.clone();
    twice.re_redact(&Redactor::default());
    assert_eq!(once, twice);
    // An F8 sidecar's own report.
    let d: SocketAddr = "10.0.0.5:443".parse().unwrap();
    let mut r = two_scopes("a.example.com:443", &format!("{MARKER}:443"));
    r.record_at(
        ScopeKey::Authority(AUTHORITY.into()),
        &Via {
            tls: true,
            sni: Some("api.example.com"),
            dst: Some(d),
        },
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 0,
            method: "POST",
            path: "/x",
            status: 200,
            head_ms: 1,
            host: None,
            grpc: false,
            observation: Observation {
                query: "key=v&ok=1",
                req_headers: &[("x-anything", "v"), ("accept", "a/b")],
                ..Observation::default()
            },
        },
    );
    let before = r.clone();
    r.re_redact(&Redactor::default());
    assert_eq!(r, before);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn shr_02_no_canary_survives_re_redaction(
        canary in "[a-z0-9]{35}",
        ua in 0usize..4,
        etag in 0usize..4,
        host_form in 0usize..2,
        sni_form in 0usize..2,
        planted_sni in any::<bool>(),
    ) {
        let prefix = ["sk-", "ghp_"];
        let host = format!("{}{canary}.example.com", prefix[host_form]);
        let sni = format!("{}{canary}.example.com", prefix[sni_form]);
        let snis: Vec<&str> = if planted_sni { vec![sni.as_str()] } else { vec![] };
        let text = plant(&leak_all(&standard(), &format!("{host}:443"), &snis), &canary, ua, etag);
        let mut r = Report::from_json(&text).unwrap();
        r.re_redact(&Redactor::default());
        assert_clean(&r, &canary, ua, etag)?;
        let mut again = r.clone();
        again.re_redact(&Redactor::default());
        prop_assert_eq!(&r, &again);
        prop_assert_eq!(r.check(256, 64), Ok(()));
    }
}
