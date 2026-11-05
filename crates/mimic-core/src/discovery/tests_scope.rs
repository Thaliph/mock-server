use std::net::SocketAddr;

use proptest::prelude::*;

use super::check::Violation;
use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Report};
use super::scope::{ScopeKey, authority_key};

fn dst(s: &str) -> SocketAddr {
    s.parse().expect("socket address")
}

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        sample_budget: 16 << 20,
    }
}

/// A fixed inbound report: ports, templates, a gRPC path, hosts, samples.
fn v1_report() -> Report {
    let l = limits();
    let mut r = Report::new(Some("pod-a".into()));
    let h = [("content-type", "application/json")];
    for (port, method, path, status, host, grpc) in [
        (
            8080u16,
            "GET",
            "/users/1",
            200u16,
            Some("users.svc:8080"),
            false,
        ),
        (8080, "GET", "/users/2", 200, Some("users.svc:8080"), false),
        (8080, "POST", "/users", 201, None, false),
        (
            9090,
            "POST",
            "/pkg.Svc/Call",
            200,
            Some("grpc.svc:9090"),
            true,
        ),
        (9090, "GET", "/healthz", 503, None, false),
    ] {
        r.record(
            &l,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Inbound,
                port,
                method,
                path,
                status,
                head_ms: 7,
                host,
                grpc,
                observation: Observation {
                    query: "a=1",
                    req_headers: &h,
                    req_body: Some(br#"{"id":1}"#),
                    resp_headers: &h,
                    resp_body: Some(br#"{"ok":true}"#),
                },
            },
        );
    }
    r
}

const V1_FIXTURE: &str = include_str!("../../tests/fixtures/report-v1.json");

/// One outbound exchange to `authority`.
fn outbound(r: &mut Report, authority: &str, path: &str, status: u16) {
    r.record_at(
        ScopeKey::Authority(authority.to_string()),
        &limits(),
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 0,
            method: "GET",
            path,
            status,
            head_ms: 3,
            host: None,
            grpc: false,
            observation: Observation::default(),
        },
    );
}

#[test]
fn egr_01_the_target_authority_wins() {
    let d = dst("10.0.0.5:8080");
    let key = |target, default, sni| authority_key(target, default, sni, d);
    assert_eq!(
        key(Some("API.Example.com."), 80, None),
        "api.example.com:80"
    );
    assert_eq!(
        key(Some("users.disc-22.svc:8080"), 80, None),
        "users.disc-22.svc:8080"
    );
    assert_eq!(
        key(Some("api.example.com"), 443, None),
        "api.example.com:443"
    );
    assert_eq!(
        key(Some("api.example.com:8443"), 443, Some("other.example.com")),
        "api.example.com:8443"
    );
    assert_eq!(
        key(Some("[2001:DB8::1]:8443"), 80, None),
        "[2001:db8::1]:8443"
    );
    assert_eq!(key(Some("[::1]"), 80, None), "[::1]:80");
    assert_eq!(key(Some("192.0.2.7"), 80, None), "192.0.2.7:80");
}

#[test]
fn egr_01_falls_back_to_the_sni_then_the_destination() {
    let d = dst("10.0.0.5:8080");
    for target in [
        None,
        Some("(invalid)"),
        Some(""),
        Some("a b:80"),
        Some("h:0"),
    ] {
        assert_eq!(
            authority_key(target, 443, Some("Api.Example.COM."), d),
            "api.example.com:8080",
            "{target:?}"
        );
        assert_eq!(
            authority_key(target, 443, None, d),
            "10.0.0.5:8080",
            "{target:?}"
        );
    }
    // An SNI outside the grammar is no better than none.
    assert_eq!(
        authority_key(None, 443, Some("bad name"), d),
        "10.0.0.5:8080"
    );
    assert_eq!(
        authority_key(None, 80, None, dst("[2001:db8::9]:9000")),
        "[2001:db8::9]:9000"
    );
}

#[test]
fn egr_01_a_detected_host_is_the_marker() {
    let d = dst("10.0.0.5:8080");
    assert_eq!(
        authority_key(Some("sk-live1234567890:8443"), 80, None, d),
        "<redacted>:8443"
    );
    assert_eq!(
        authority_key(None, 443, Some("sk-live1234567890"), d),
        "<redacted>:8080"
    );
}

#[test]
fn egr_01_scope_keys_round_trip_through_their_text() {
    for (text, key) in [
        ("8080", ScopeKey::Port(8080)),
        (
            "api.example.com:443",
            ScopeKey::Authority("api.example.com:443".into()),
        ),
        ("[::1]:80", ScopeKey::Authority("[::1]:80".into())),
        (
            "<redacted>:443",
            ScopeKey::Authority("<redacted>:443".into()),
        ),
    ] {
        assert_eq!(ScopeKey::parse(text), Some(key.clone()), "{text}");
        assert_eq!(key.to_string(), text);
    }
    for bad in [
        "",
        "api.example.com",
        "API.example.com:80",
        "a.com:0",
        "a.com:080",
        "a.com:65536",
        "a b:80",
        "a.com.:80",
        "[2001:DB8::1]:80",
        "[::1",
        "x:80:80",
        "08080",
        "70000",
        "a\u{0}.com:80",
        "-a.com:80",
        "a..com:80",
        "a.com:+80",
        "[::1]:+80",
        "+80",
        "a.com:",
        "a.com:00",
        "a.com:0",
        "a.com:-1",
        "a.com: 80",
        "0",
    ] {
        assert_eq!(ScopeKey::parse(bad), None, "{bad:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn egr_01_every_key_matches_the_grammar(
        target in proptest::option::of(".{0,300}"),
        sni in proptest::option::of(".{0,300}"),
        ip in any::<std::net::IpAddr>(),
        port in any::<u16>(),
        default in prop_oneof![Just(80u16), Just(443u16)],
    ) {
        let key = authority_key(target.as_deref(), default, sni.as_deref(), SocketAddr::new(ip, port));
        prop_assert!(key.len() <= 261, "{key:?}");
        prop_assert!(!key.chars().any(|c| c.is_control() || c == ' '), "{key:?}");
        prop_assert!(matches!(ScopeKey::parse(&key), Some(ScopeKey::Authority(k)) if k == key), "{key:?}");
    }
}

#[test]
fn egr_03_a_report_without_outbound_is_byte_identical_to_f5() {
    let json = v1_report().to_json().unwrap();
    assert_eq!(json, V1_FIXTURE);
    assert!(json.starts_with("{\"version\":1,"));
    let back = Report::from_json(V1_FIXTURE).unwrap();
    assert_eq!(back.to_json().unwrap(), V1_FIXTURE);
    assert_eq!(back, v1_report());
}

#[test]
fn egr_03_an_outbound_exchange_makes_version_2() {
    let mut r = v1_report();
    outbound(&mut r, "api.example.com:443", "/v1/charges", 200);
    let json = r.to_json().unwrap();
    assert!(json.starts_with("{\"version\":2,"), "{json}");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let scope = &value["scopes"]["outbound"]["api.example.com:443"];
    assert!(scope["root"].is_object(), "{scope}");
    assert!(scope["origin"].is_object(), "{scope}");
    assert!(scope["tunnels"].is_object(), "{scope}");
    // The inbound part is what version 1 holds.
    assert_eq!(
        serde_json::to_string(&value["scopes"]["inbound"]).unwrap(),
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(V1_FIXTURE).unwrap()["scopes"]["inbound"]
        )
        .unwrap()
    );
    let back = Report::from_json(&json).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.to_json().unwrap(), json);
    assert_eq!(
        back.endpoints()
            .iter()
            .filter(|e| e.direction == Direction::Outbound)
            .count(),
        1
    );
}

#[test]
fn egr_03_merging_keeps_the_version_in_step_with_the_content() {
    let l = limits();
    let mut out = Report::new(None);
    outbound(&mut out, "a.example.com:80", "/x", 200);
    let merged = super::report::merge(Report::new(None), out.clone(), &l);
    assert!(merged.to_json().unwrap().starts_with("{\"version\":2,"));
    let merged = super::report::merge(v1_report(), Report::new(Some("pod-a".into())), &l);
    assert_eq!(merged.to_json().unwrap(), V1_FIXTURE);
}

#[test]
fn egr_03_from_json_refuses_what_the_format_does_not_allow() {
    let mut r = v1_report();
    outbound(&mut r, "api.example.com:443", "/v1/charges", 200);
    let v2 = r.to_json().unwrap();
    // Version 1 with an outbound scope.
    assert!(Report::from_json(&v2.replacen("\"version\":2", "\"version\":1", 1)).is_err());
    // Version 2 without one.
    assert!(Report::from_json(&V1_FIXTURE.replacen("\"version\":1", "\"version\":2", 1)).is_err());
    // Version 3.
    assert!(Report::from_json(&v2.replacen("\"version\":2", "\"version\":3", 1)).is_err());
    // An inbound key that is not a port.
    assert!(
        Report::from_json(&V1_FIXTURE.replacen("\"8080\"", "\"api.example.com:80\"", 1)).is_err()
    );
    // Outbound keys outside the grammar.
    for bad in ["api.example.com", "API.example.com:443", "a b:443", "8080"] {
        let json = v2.replacen("api.example.com:443", bad, 1);
        assert!(Report::from_json(&json).is_err(), "{bad}");
    }
}

#[test]
fn egr_03_the_push_check_still_refuses_version_2() {
    let mut r = v1_report();
    assert_eq!(r.check(256), Ok(()));
    outbound(&mut r, "api.example.com:443", "/v1/charges", 200);
    let err = r.check(256).unwrap_err();
    assert_eq!(err, Violation::Version);
    let err = Report::parse_checked(&r.to_json().unwrap(), 256).unwrap_err();
    assert_eq!(err.reason(), "version");
    assert!(Report::parse_checked(V1_FIXTURE, 256).is_ok());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn egr_01_parse_is_canonical(s in "[0-9a-z:.\\[\\]+<>_ -]{0,30}") {
        if let Some(k) = ScopeKey::parse(&s) {
            prop_assert_eq!(k.to_string(), s);
        }
    }
}

fn v2_json() -> String {
    let mut r = v1_report();
    outbound(&mut r, "api.example.com:443", "/v1/charges", 200);
    r.to_json().unwrap()
}

fn with_scope_field(
    json: &str,
    direction: &str,
    key: &str,
    field: &str,
    value: serde_json::Value,
) -> String {
    let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
    v["scopes"][direction][key][field] = value;
    v.to_string()
}

#[test]
fn egr_03_from_json_bounds_origin_and_tunnels() {
    let json = v2_json();
    let key = "api.example.com:443";
    let many: Vec<String> = (0..17).map(|i| format!("h{i:02}.example.com")).collect();
    for (what, field, value) in [
        (
            "17 sni",
            "origin",
            serde_json::json!({"schemes": {}, "sni": many, "dst": []}),
        ),
        (
            "17 dst",
            "origin",
            serde_json::json!({"schemes": {}, "sni": [], "dst": many}),
        ),
        (
            "scheme",
            "origin",
            serde_json::json!({"schemes": {"ftp": 1}, "sni": [], "dst": []}),
        ),
        (
            "long sni",
            "origin",
            serde_json::json!({"schemes": {}, "sni": ["a".repeat(300)], "dst": []}),
        ),
        (
            "unknown field",
            "origin",
            serde_json::json!({"schemes": {}, "sni": [], "dst": [], "x": 1}),
        ),
        ("kind", "tunnels", serde_json::json!({"made_up": 1})),
    ] {
        let bad = with_scope_field(&json, "outbound", key, field, value);
        assert!(Report::from_json(&bad).is_err(), "{what}");
    }
    let ok = with_scope_field(
        &json,
        "outbound",
        key,
        "tunnels",
        serde_json::json!({"tcp": 2, "tls_no_ca": 1}),
    );
    assert!(Report::from_json(&ok).is_ok());
}

#[test]
fn egr_03_from_json_refuses_origin_and_tunnels_on_inbound_scopes() {
    for (field, value) in [
        (
            "origin",
            serde_json::json!({"schemes": {}, "sni": [], "dst": []}),
        ),
        ("tunnels", serde_json::json!({"tcp": 1})),
    ] {
        let bad = with_scope_field(V1_FIXTURE, "inbound", "8080", field, value.clone());
        assert!(Report::from_json(&bad).is_err(), "{field}");
        assert_eq!(
            Report::parse_checked(&bad, 256).unwrap_err(),
            Violation::Malformed,
            "{field}"
        );
    }
}

#[test]
fn egr_03_one_version_rule_everywhere() {
    // A port-keyed outbound scope is version 1 and is no valid report: `from_json` refuses it.
    let l = limits();
    let mut r = Report::new(None);
    r.record(
        &l,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 80,
            method: "GET",
            path: "/x",
            status: 200,
            head_ms: 1,
            host: None,
            grpc: false,
            observation: Observation::default(),
        },
    );
    let json = r.to_json().unwrap();
    assert!(json.starts_with("{\"version\":1,"));
    assert!(Report::from_json(&json).is_err());
    // Version 2 holds exactly when an authority scope does.
    assert!(Report::from_json(&v2_json()).is_ok());
}
