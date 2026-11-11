//! PLH-01 to PLH-05 (spec §6, §12.15): typed placeholders and the placeholder upgrade.
//! Canaries never appear in assertion messages.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};

use super::observed::Observation;
use super::placeholder::{self, PLACEHOLDERS};
use super::redact::{MARKER, Redactor, segment_rule, value_rule};
use super::report::{Direction, Exchange, Limits, Report, Via};
use super::scope::ScopeKey;
use super::tests_reredact_outbound::hash_free;

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

fn exchange<'a>(direction: Direction, path: &'a str, observation: Observation<'a>) -> Exchange<'a> {
    Exchange {
        direction,
        port: 8080,
        method: "POST",
        path,
        status: 200,
        head_ms: 1,
        host: None,
        grpc: false,
        observation,
    }
}

/// A report holding one recorded exchange.
fn recorded(
    redactor: &Redactor,
    direction: Direction,
    path: &str,
    observation: Observation<'_>,
) -> Report {
    let mut r = Report::new(None);
    r.record(&limits(), redactor, &exchange(direction, path, observation));
    r
}

fn shannon(s: &str) -> f64 {
    let mut counts = BTreeMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let n = s.chars().count() as f64;
    counts
        .values()
        .map(|&k| {
            let p = k as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn percent_encoded(s: &str) -> String {
    s.bytes().map(|b| format!("%{b:02x}")).collect()
}

#[test]
fn plh_01_the_placeholders_are_the_thirteen_of_the_spec() {
    assert_eq!(
        PLACEHOLDERS,
        [
            "mimic-auth",
            "mimic-token",
            "mimic-jwt",
            "mimic-api-key",
            "mimic-password",
            "mimic-private-key",
            "mimic-secret",
            "mimic-session",
            "mimic-cookie",
            "mimic-signature",
            "mimic-card",
            "mimic-iban",
            "mimic-ssn",
        ]
    );
}

#[test]
fn plh_01_grammar_and_no_detector_catches_a_placeholder() {
    for p in PLACEHOLDERS {
        let rest = p.strip_prefix("mimic-").expect("prefix");
        assert!(
            rest.split('-')
                .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase())),
            "{p}"
        );
        assert!(rest.split('-').count() <= 2, "{p}");
        for form in [
            p.to_string(),
            format!("Bearer {p}"),
            format!("Basic {p}"),
            percent_encoded(p),
            format!("a={p}"),
        ] {
            assert!(value_rule(&form).is_none(), "value {form}");
            assert!(segment_rule(&form).is_none(), "segment {form}");
        }
        assert!(shannon(p) < 3.5, "{p}: {}", shannon(p));
        // Unchanged by redaction, in every place a value sits.
        let r = Redactor::default();
        assert_eq!(r.field("note", p), p);
        assert_eq!(r.header("x-note", p), p);
        assert_eq!(redact_path(&format!("/a/{p}")), format!("/a/{p}"));
    }
}

fn redact_path(path: &str) -> String {
    super::redact::path(path)
}

#[test]
fn plh_01_a_placeholder_segment_is_a_literal() {
    for p in PLACEHOLDERS {
        let path = format!("/v1/{p}");
        let r = recorded(
            &Redactor::default(),
            Direction::Inbound,
            &path,
            Observation::default(),
        );
        assert_eq!(r.endpoints()[0].template, path, "{p}");
    }
}

/// What a value the redactor replaces may look like.
fn is_placeholder_form(out: &str) -> bool {
    PLACEHOLDERS.iter().any(|p| {
        out == *p
            || out == format!("Bearer {p}")
            || out == format!("Basic {p}")
            || out == format!("a={p}")
            || out == format!("/x/{p}")
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn plh_01_every_replaced_value_is_a_placeholder(
        canary in "[A-Za-z0-9]{32}",
        place in 0usize..14,
    ) {
        let r = Redactor::default();
        let jwt = format!("eyJhbGciOiJIUzI1NiJ9.{canary}.c2ln");
        let out = match place {
            0 => r.header("authorization", &format!("Bearer {canary}")),
            1 => r.header("authorization", &format!("Basic {canary}")),
            2 => r.header("cookie", &format!("a={canary}")),
            3 => r.header("x-api-key", &canary),
            4 => r.header("x-note", &jwt),
            5 => r.field("token", &canary),
            6 => r.field("note", &format!("ghp_{canary}")),
            7 => r.field("note", &format!("Bearer {canary}")),
            8 => r.field("note", &format!("sk-{canary}")),
            9 => format!("/x/{}", super::redact::segment(&jwt)),
            10 => r.header("x-note", &format!("AIza{}", &canary[..31]).replace("AIza", "AIzaSy")),
            11 => {
                let mut v = json!({"note": jwt, "password": canary});
                r.json(&mut v);
                v["note"].as_str().unwrap_or_default().to_string()
            }
            12 => r.header("authorization", &canary),
            _ => {
                let mut v = json!({"password": {"deep": [canary]}});
                r.json(&mut v);
                v["password"].as_str().unwrap_or_default().to_string()
            }
        };
        prop_assert!(is_placeholder_form(&out) || out.contains("mimic-"), "place {place}: {out}");
        prop_assert!(!out.contains(&canary) && !out.contains(MARKER));
        let text = serde_json::to_string(&out).unwrap();
        prop_assert!(hash_free(&text, &canary));
        // One of the 13, whatever the place (checked as a whole word).
        prop_assert!(placeholder::is_marked(&out), "place {place}");
        prop_assert!(PLACEHOLDERS.iter().any(|p| out.ends_with(p)));
    }
}

fn header(r: &Redactor, name: &str, value: &str) -> String {
    r.header(name, value)
}

fn json_of(r: &Redactor, key: &str, value: Value) -> Value {
    let mut v = json!({ key: value });
    r.json(&mut v);
    v[key].clone()
}

#[test]
fn plh_02_type_by_name() {
    let r = Redactor::new(["tenantcode"]);
    let by_name = [
        ("authorization", "Bearer x", "Bearer mimic-token"),
        ("Authorization", "bearer x", "Bearer mimic-token"),
        ("Authorization", "Basic x", "Basic mimic-auth"),
        ("Authorization", "NotAScheme x", "mimic-auth"),
        ("Authorization", "Digest realm=r", "Digest mimic-auth"),
        ("Proxy-Authorization", "x", "mimic-auth"),
        ("token", "x", "mimic-token"),
        ("X-Api-Key", "x", "mimic-api-key"),
        ("password", "x", "mimic-password"),
        ("passwd", "x", "mimic-password"),
        ("private_key", "x", "mimic-private-key"),
        ("client_secret", "x", "mimic-secret"),
        ("Session-Id", "x", "mimic-session"),
        ("Cookie", "a=1", "a=mimic-cookie"),
        ("signature", "x", "mimic-signature"),
        ("card_number", "x", "mimic-card"),
        ("pan", "x", "mimic-card"),
        ("cvv", "x", "mimic-card"),
        ("iban", "x", "mimic-iban"),
        ("ssn", "x", "mimic-ssn"),
        ("credential", "x", "mimic-auth"),
        ("tenant_code", "x", "mimic-secret"),
    ];
    for (name, value, want) in by_name {
        assert_eq!(header(&r, name, value), want, "header {name}");
        // A query value, a form field and a JSON string give the placeholder of the name alone.
        let alone = want.rsplit(['=', ' ']).next().unwrap_or(want);
        let alone = if name.eq_ignore_ascii_case("authorization") {
            "mimic-auth"
        } else {
            alone
        };
        assert_eq!(r.field(name, value), alone, "field {name}");
        assert_eq!(json_of(&r, name, json!(value)), json!(alone), "json {name}");
    }
}

#[test]
fn plh_02_type_by_value_under_a_neutral_name() {
    let r = Redactor::default();
    let jwt = "eyJhbGciOiJIUzI1NiJ9.e30.c2ln";
    let pem = "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----";
    let cases = [
        (jwt, "mimic-jwt"),
        (pem, "mimic-private-key"),
        ("Bearer x", "mimic-auth"),
        ("Basic x", "mimic-auth"),
        ("4111 1111 1111 1111", "mimic-card"),
        ("4111-1111-1111-1111", "mimic-card"),
        ("AKIAIOSFODNN7EXAMPLE", "mimic-api-key"),
        ("AIzaSyA1234567890abcdefghijklmnopqrstuv", "mimic-api-key"),
        ("sk-abc", "mimic-api-key"),
        ("sk_live_abc", "mimic-api-key"),
        ("rk_live_abc", "mimic-api-key"),
        ("ghp_abc", "mimic-token"),
        ("glpat-abc", "mimic-token"),
        ("xoxb-abc", "mimic-token"),
        ("github_pat_abc", "mimic-token"),
        ("https://u:p@h/", "mimic-password"),
    ];
    for (value, want) in cases {
        assert_eq!(r.field("note", value), want, "{want}");
        assert_eq!(r.header("x-note", value), want, "{want}");
        assert_eq!(json_of(&r, "note", json!(value)), json!(want), "{want}");
    }
    assert_eq!(redact_path(&format!("/reset/{jwt}")), "/reset/mimic-jwt");
    assert_eq!(redact_path("/reset/%65yJhbGciOiJIUzI1NiJ9.e30.c2ln"), "/reset/mimic-jwt");
    assert_eq!(redact_path("/pay/4111111111111111"), "/pay/mimic-card");
    assert_eq!(json_of(&r, "note", json!(4111111111111111u64)), json!("mimic-card"));
    assert_eq!(
        r.header("location", &format!("https://h/reset/{jwt}?code=C&access_token=T")),
        "https://h/reset/mimic-jwt?code=C&access_token=mimic-token"
    );
}

#[test]
fn plh_02_the_outbound_view() {
    let r = Redactor::default().outbound_request();
    assert_eq!(r.header("X-Tenant-Key", "v"), "mimic-secret");
    assert_eq!(r.header("X-Anything", "v"), "mimic-secret");
    assert_eq!(r.header("accept", "v"), "v");
    assert_eq!(r.header("Authorization", "Basic v"), "Basic mimic-auth");
    assert_eq!(r.header("Cookie", "a=v"), "a=mimic-cookie");
    assert_eq!(r.field("key", "v"), "mimic-api-key");
    assert_eq!(r.field("sig", "v"), "mimic-signature");
    assert_eq!(r.field("idempotency_key", "v"), "mimic-api-key");
    let mut v = json!({"idempotency_key": "v"});
    r.json(&mut v);
    assert_eq!(v, json!({"idempotency_key": "mimic-api-key"}));
}

#[test]
fn plh_03_precedence() {
    let r = Redactor::new(["tenantcode"]);
    let jwt = "eyJhbGciOiJIUzI1NiJ9.e30.c2ln";
    assert_eq!(r.field("token", jwt), "mimic-token");
    assert_eq!(r.field("note", jwt), "mimic-jwt");
    for name in ["session_token", "auth_token", "csrftoken", "refreshToken"] {
        assert_eq!(r.field(name, "x"), "mimic-token", "{name}");
    }
    assert_eq!(r.field("token_secret", "x"), "mimic-secret");
    assert_eq!(r.field("JSESSIONID", "x"), "mimic-session");
    assert_eq!(
        r.header("Set-Cookie", "s=1; Path=/"),
        "s=mimic-cookie; Path=/"
    );
    assert_eq!(r.header("Set-Cookie", "bare"), "mimic-cookie");
    assert_eq!(r.header("Proxy-Authorization", "x"), "mimic-auth");
    let mut v = json!({"password": {"a": [1, {"b": 2}]}, "n": 3});
    r.json(&mut v);
    assert_eq!(v, json!({"password": "mimic-password", "n": 3}));
    let mut v = json!({"password": 12345});
    r.json(&mut v);
    assert_eq!(v, json!({"password": "mimic-password"}));
}

#[test]
fn plh_03_the_output_does_not_depend_on_the_secret() {
    let at = |canary: &str| {
        let headers = [
            ("authorization", format!("Bearer {canary}")),
            ("x-api-key", canary.to_string()),
        ];
        let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let body = format!(r#"{{"password":"{canary}","note":"ghp_{canary}"}}"#);
        recorded(
            &Redactor::default(),
            Direction::Inbound,
            "/x",
            Observation {
                query: &format!("token={canary}"),
                req_headers: &headers,
                req_body: Some(body.as_bytes()),
                resp_headers: &[],
                resp_body: None,
            },
        )
        .to_json()
        .unwrap()
    };
    let short = at("cnry8f3a1b7d");
    let long = at("cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4c6d8e");
    assert_eq!(short.replace("cnry8f3a1b7d", ""), short);
    assert_eq!(short, long);
}

/// A report that records the inputs of the canaries, inbound.
fn leaky(redactor: &Redactor, canary: &str) -> Report {
    let jwt = format!("eyJhbGciOiJIUzI1NiJ9.{canary}.c2ln");
    let headers = [
        ("authorization", format!("Bearer {canary}")),
        ("cookie", format!("a={canary}")),
        ("x-api-key", canary.to_string()),
        ("x-custom-code", canary.to_string()),
        ("x-note", jwt.clone()),
        ("location", format!("https://h/cb/{jwt}?code={canary}&token={canary}")),
    ];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let body = format!(
        r#"{{"password":"{canary}","custom_code":"{canary}","note":"{jwt}","card":4111111111111111}}"#
    );
    recorded(
        redactor,
        Direction::Inbound,
        &format!("/reset/{jwt}"),
        Observation {
            query: &format!("token={canary}&tenant_code={canary}&k=ghp_{canary}"),
            req_headers: &headers,
            req_body: Some(body.as_bytes()),
            resp_headers: &headers[..2],
            resp_body: Some(body.as_bytes()),
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn plh_04_re_redaction_is_idempotent(canary in "[A-Za-z0-9]{32}") {
        let redactor = Redactor::new(["tenantcode", "customcode"]);
        let mut r = leaky(&redactor, &canary);
        let once = r.to_json().unwrap();
        prop_assert!(hash_free(&once, &canary));
        prop_assert!(!once.contains(MARKER));
        r.re_redact(&redactor);
        prop_assert_eq!(&once, &r.to_json().unwrap());
        r.re_redact(&redactor);
        prop_assert_eq!(&once, &r.to_json().unwrap());
    }
}

#[test]
fn plh_04_a_name_known_only_to_the_controller_gives_the_sidecar_form() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let names = ["tenantcode", "customcode"];
    let sidecar = leaky(&Redactor::new(names), canary);
    let mut controller = leaky(&Redactor::default(), canary);
    controller.re_redact(&Redactor::new(names));
    let (a, b) = (sidecar.to_json().unwrap(), controller.to_json().unwrap());
    assert!(hash_free(&b, canary));
    assert!(b.contains("mimic-secret"));
    let sketches = |text: &str| {
        let v: Value = serde_json::from_str(text).unwrap();
        v["scopes"]["inbound"]["8080"]["root"]["literals"]["reset"]["param"]["endpoints"]["POST"]
            ["observed"]["fields"]["fields"]
            .clone()
    };
    assert_ne!(sketches(&a), Value::Null);
    assert_eq!(sketches(&a), sketches(&b));
}

/// The legacy form of a report: every placeholder swapped for the marker, as the F12 recorder
/// wrote it (a sample key is stale then, which the upgrade recomputes).
fn legacy(report: &Report) -> Report {
    let mut text = report.to_json().unwrap();
    for p in PLACEHOLDERS {
        text = text.replace(p, MARKER);
    }
    Report::from_json(&text).unwrap()
}

fn sample_values(r: &Report) -> Vec<String> {
    let mut out = Vec::new();
    for e in r.endpoints() {
        for (_, s) in e.stats.samples().iter() {
            out.push(s.path().to_string());
            out.extend(s.query().iter().map(|(_, v)| v.clone()));
            out.extend(s.request_headers().iter().map(|(_, v)| v.clone()));
            out.extend(s.response_headers().iter().map(|(_, v)| v.clone()));
        }
    }
    out
}

#[test]
fn plh_05_the_upgrade_gives_the_type_of_the_field() {
    let canary = "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4";
    let headers = [
        ("authorization", "Bearer x"),
        ("x-api-key", "x"),
        ("cookie", "a=1; b=2"),
        ("x-note", "ghp_x"),
        ("proxy-authorization", "Basic x"),
    ];
    let body = br#"{"password":"p","card_number":[1,2],"note":"ghp_x"}"#;
    let original = recorded(
        &Redactor::default(),
        Direction::Inbound,
        "/reset/eyJhbGciOiJIUzI1NiJ9.e30.c2ln",
        Observation {
            query: "token=t&k=ghp_x",
            req_headers: &headers,
            req_body: Some(body),
            resp_headers: &[],
            resp_body: None,
        },
    );
    let mut old = legacy(&original);
    assert!(old.to_json().unwrap().contains(MARKER));
    placeholder::upgrade(&mut old);
    let values = sample_values(&old);
    for want in [
        "Bearer mimic-token",
        "mimic-api-key",
        "a=mimic-cookie; b=mimic-cookie",
        "Basic mimic-auth",
        "mimic-token",
        "/reset/mimic-secret",
        "mimic-secret",
    ] {
        assert!(values.iter().any(|v| v == want), "{want}: {values:?}");
    }
    let text = old.to_json().unwrap();
    assert!(!text.contains(MARKER));
    assert!(text.contains("\"password\":\"mimic-password\""));
    assert!(text.contains("\"card_number\":\"mimic-card\""));
    assert!(text.contains("\"note\":\"mimic-secret\""));
    // Keys and sizes are recomputed: the upgraded report is a valid one.
    let json = old.to_json().unwrap();
    assert!(Report::parse_checked(&json, 256, 64).is_ok());
    let _ = canary;
}

#[test]
fn plh_05_the_upgrade_is_idempotent_and_leaves_a_current_report_alone() {
    let current = leaky(&Redactor::default(), "cnry8f3a1b7d9e2c4a6f0b5d7e1c3a9f2b4");
    let before = current.to_json().unwrap();
    let mut same = current.clone();
    placeholder::upgrade(&mut same);
    assert_eq!(before, same.to_json().unwrap());
    let mut old = legacy(&current);
    placeholder::upgrade(&mut old);
    let once = old.to_json().unwrap();
    placeholder::upgrade(&mut old);
    assert_eq!(once, old.to_json().unwrap());
}

#[test]
fn plh_05_an_outbound_request_and_an_authority_host() {
    let headers = [("x-tenant-key", "v"), ("authorization", "Bearer v")];
    let via = Via {
        tls: false,
        sni: None,
        dst: None,
    };
    let mut r = Report::new(None);
    r.record_at(
        ScopeKey::Authority("api.example.com:443".into()),
        &via,
        &limits(),
        &Redactor::default().outbound_request(),
        &exchange(
            Direction::Outbound,
            "/x",
            Observation {
                query: "sig=s&ok=1",
                req_headers: &headers,
                req_body: None,
                resp_headers: &[],
                resp_body: None,
            },
        ),
    );
    let mut old = legacy(&r);
    // The authority of a redacted host keeps the legacy marker.
    let text = old
        .to_json()
        .unwrap()
        .replace("api.example.com:443", "<redacted>:443");
    old = Report::from_json(&text).unwrap();
    placeholder::upgrade(&mut old);
    let upgraded = old.to_json().unwrap();
    assert_eq!(upgraded.matches(MARKER).count(), 1, "the authority only");
    assert!(upgraded.contains("\"<redacted>:443\""));
    let values = sample_values(&old);
    for want in ["mimic-secret", "Bearer mimic-token", "mimic-signature"] {
        assert!(values.iter().any(|v| v == want), "{want}: {values:?}");
    }
}

/// The F12 golden reports, upgraded: no marker but in authority hosts, a valid report, and
/// the upgrade is idempotent.
#[test]
fn plh_05_the_golden_reports_of_f12() {
    for case in [
        "single-below",
        "merged-below",
        "merged-endpoint-limit",
        "merged-authority-limit",
        "merged-sample-budget",
    ] {
        let path = format!(
            "{}/tests/fixtures/f12-golden/{case}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).unwrap();
        let mut r = Report::from_json(&text).unwrap();
        assert!(r.to_json().unwrap().contains(MARKER), "{case}");
        placeholder::upgrade(&mut r);
        let once = r.to_json().unwrap();
        let v: Value = serde_json::from_str(&once).unwrap();
        let mut markers = 0;
        scan(&v, &mut markers);
        assert_eq!(markers, 0, "{case}");
        assert!(Report::parse_checked(&once, 256, 64).is_ok(), "{case}");
        placeholder::upgrade(&mut r);
        assert_eq!(once, r.to_json().unwrap(), "{case}");
    }
}

/// Counts the strings that hold the marker outside the keys of `scopes.outbound` and `hosts`.
fn scan(v: &Value, n: &mut usize) {
    match v {
        Value::String(s) if s.contains(MARKER) => *n += 1,
        Value::Array(a) => a.iter().for_each(|x| scan(x, n)),
        Value::Object(o) => {
            for (k, x) in o {
                if k == "hosts" {
                    continue;
                }
                if k.contains(MARKER) && !k.ends_with(":443") && !k.contains(':') {
                    *n += 1;
                }
                scan(x, n);
            }
        }
        _ => {}
    }
}
