//! SHR-01: the structural check of a pushed report with outbound scopes (spec §7.3, §6.1).

use proptest::prelude::*;
use serde_json::{Value, json};

use super::check::Violation;
use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Report, TUNNEL_KINDS, Via, merge};
use super::scope::ScopeKey;
use super::tests_scope::{V1_FIXTURE, dst, limits, outbound, v1_report};

const PUSH: usize = 256;
const AUTH: usize = 64;
const KEY: &str = "api.example.com:443";

fn v2_report() -> Report {
    let mut r = v1_report();
    outbound(&mut r, KEY, "/v1/charges", 200);
    r
}

fn v2_value() -> Value {
    serde_json::from_str(&v2_report().to_json().unwrap()).unwrap()
}

fn verdict(v: &Value) -> Result<(), Violation> {
    Report::parse_checked(&v.to_string(), PUSH, AUTH).map(|_| ())
}

fn outbound_scope(v: &mut Value) -> &mut Value {
    &mut v["scopes"]["outbound"][KEY]
}

fn each_endpoint(v: &mut Value, f: &mut impl FnMut(&mut Value)) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Object(endpoints)) = map.get_mut("endpoints") {
                endpoints.values_mut().for_each(&mut *f);
            }
            map.values_mut().for_each(|c| each_endpoint(c, f));
        }
        Value::Array(items) => items.iter_mut().for_each(|c| each_endpoint(c, f)),
        _ => {}
    }
}

fn rekey(v: &mut Value, to: &str) {
    let outbound = v["scopes"]["outbound"].as_object_mut().unwrap();
    let scope = outbound.remove(KEY).unwrap();
    outbound.insert(to.to_string(), scope);
}

#[test]
fn shr_01_accepts_version_2_with_an_outbound_scope() {
    let r = v2_report();
    assert_eq!(r.version(), 2);
    assert_eq!(r.check(PUSH, AUTH), Ok(()));
    let parsed = Report::parse_checked(&r.to_json().unwrap(), PUSH, AUTH).unwrap();
    assert_eq!(parsed, r);
    // A tunnel-only scope is a scope.
    let mut t = Report::new(None);
    t.record_tunnel(&limits(), ScopeKey::Authority(KEY.into()), "tcp");
    assert_eq!(t.version(), 2);
    assert_eq!(t.check(PUSH, AUTH), Ok(()));
    // Version 1 as CTL-01 has it.
    assert!(Report::parse_checked(V1_FIXTURE, PUSH, AUTH).is_ok());
    assert_eq!(v1_report().check(PUSH, AUTH), Ok(()));
}

#[test]
fn shr_01_the_version_rule() {
    let mut no_outbound: Value = serde_json::from_str(V1_FIXTURE).unwrap();
    no_outbound["version"] = json!(2);
    assert_eq!(verdict(&no_outbound), Err(Violation::Version));
    let mut v1_with = v2_value();
    v1_with["version"] = json!(1);
    assert_eq!(verdict(&v1_with), Err(Violation::Version));
    let mut v3 = v2_value();
    v3["version"] = json!(3);
    assert_eq!(verdict(&v3), Err(Violation::Version));
    assert_eq!(Violation::Version.reason(), "version");
}

#[test]
fn shr_01_an_outbound_key_outside_the_grammar_is_authority() {
    let long = format!("{}:80", "a".repeat(254));
    for bad in [
        "A.example:80",
        "a.example:080",
        "a.example:0",
        "a.example",
        "8080",
        long.as_str(),
    ] {
        let mut v = v2_value();
        rekey(&mut v, bad);
        let err = verdict(&v).unwrap_err();
        assert_eq!(err, Violation::Authority, "{bad}");
        assert_eq!(err.reason(), "authority");
    }
    for good in ["a.example:80", "<redacted>:443", "[::1]:8080", "10.0.0.1:1"] {
        let mut v = v2_value();
        rekey(&mut v, good);
        assert_eq!(verdict(&v), Ok(()), "{good}");
    }
    // An inbound scope keyed by an authority stays malformed.
    let mut v: Value = serde_json::from_str(V1_FIXTURE).unwrap();
    let scope = v["scopes"]["inbound"]["8080"].take();
    v["scopes"]["inbound"] = json!({ "a.example:80": scope });
    assert_eq!(verdict(&v), Err(Violation::Malformed));
}

#[test]
fn shr_01_origin_bounds() {
    let many =
        |n: usize| -> Vec<String> { (0..n).map(|i| format!("h{i:02}.example.com")).collect() };
    let dsts = |n: usize| -> Vec<String> { (0..n).map(|i| format!("10.0.0.{i}:80")).collect() };
    let long = "a".repeat(63);
    let too_long = format!("{long}.{long}.{long}.{long}.example.com");
    assert!(too_long.len() > 253);
    let origin = |schemes: Value, sni: Vec<String>, dst: Vec<String>| json!({"schemes": schemes, "sni": sni, "dst": dst});
    let bad = [
        ("17 sni", origin(json!({}), many(17), vec![])),
        ("17 dst", origin(json!({}), vec![], dsts(17))),
        (
            "upper-case sni",
            origin(json!({}), vec!["API.example.com".into()], vec![]),
        ),
        ("long sni", origin(json!({}), vec![too_long], vec![])),
        (
            "dst without port",
            origin(json!({}), vec![], vec!["10.0.0.1".into()]),
        ),
        (
            "dst with a name",
            origin(json!({}), vec![], vec!["a.example:80".into()]),
        ),
        (
            "dst port 0",
            origin(json!({}), vec![], vec!["10.0.0.1:0".into()]),
        ),
        ("scheme ftp", origin(json!({"ftp": 1}), vec![], vec![])),
    ];
    for (what, value) in bad {
        let mut v = v2_value();
        outbound_scope(&mut v)["origin"] = value;
        let err = verdict(&v).unwrap_err();
        assert_eq!(err, Violation::Origin, "{what}");
        assert_eq!(err.reason(), "origin");
    }
    let mut ok = v2_value();
    outbound_scope(&mut ok)["origin"] = origin(
        json!({"http": 2, "https": 1}),
        many(16),
        vec!["10.0.0.1:80".into(), "[2001:db8::1]:443".into()],
    );
    assert_eq!(verdict(&ok), Ok(()));
}

#[test]
fn shr_01_tunnel_kinds() {
    let mut v = v2_value();
    outbound_scope(&mut v)["tunnels"] = json!({"udp": 1});
    let err = verdict(&v).unwrap_err();
    assert_eq!(err, Violation::Tunnels);
    assert_eq!(err.reason(), "tunnels");
    let all: serde_json::Map<String, Value> = TUNNEL_KINDS
        .iter()
        .map(|k| ((*k).into(), json!(1)))
        .collect();
    outbound_scope(&mut v)["tunnels"] = Value::Object(all);
    assert_eq!(verdict(&v), Ok(()));
}

#[test]
fn shr_01_hosts_on_an_outbound_endpoint() {
    let mut v = v2_value();
    each_endpoint(outbound_scope(&mut v), &mut |e| {
        e["hosts"] = json!(["svc"]);
    });
    assert_eq!(verdict(&v), Err(Violation::Hosts));
}

#[test]
fn shr_01_origin_and_tunnels_on_an_inbound_scope_are_malformed() {
    for (field, value) in [
        ("origin", json!({"schemes": {}, "sni": [], "dst": []})),
        ("tunnels", json!({"tcp": 1})),
    ] {
        let mut v = v2_value();
        v["scopes"]["inbound"]["8080"][field] = value;
        assert_eq!(verdict(&v), Err(Violation::Malformed), "{field}");
    }
}

fn tunnel_only(n: usize) -> Report {
    let mut r = Report::new(None);
    for i in 0..n {
        r.record_tunnel(
            &limits(),
            ScopeKey::Authority(format!("h{i}.example.com:443")),
            "tcp",
        );
    }
    r
}

fn exchange_of(direction: Direction, path: &str) -> Exchange<'_> {
    Exchange {
        direction,
        port: 80,
        method: "GET",
        path,
        status: 200,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: Observation::default(),
    }
}

#[test]
fn shr_01_push_limits_are_per_direction_and_for_authorities() {
    let mut r = Report::new(None);
    for p in ["/a", "/b", "/c"] {
        r.record(
            &limits(),
            &Redactor::default(),
            &exchange_of(Direction::Inbound, p),
        );
    }
    let via = Via {
        tls: false,
        sni: None,
        dst: None,
    };
    let out = |r: &mut Report, path: &str| {
        r.record_at(
            ScopeKey::Authority(KEY.into()),
            &via,
            &limits(),
            &Redactor::default(),
            &exchange_of(Direction::Outbound, path),
        );
    };
    for p in ["/a", "/b"] {
        out(&mut r, p);
    }
    assert_eq!(r.check(3, AUTH), Ok(()), "3 inbound and 2 outbound");
    out(&mut r, "/c");
    assert_eq!(r.check(3, AUTH), Ok(()), "3 and 3");
    out(&mut r, "/d");
    let err = r.check(3, AUTH).unwrap_err();
    assert_eq!(err, Violation::PushLimit);
    assert!(err.is_push_limit());
    let err = Report::parse_checked(&r.to_json().unwrap(), 3, AUTH).unwrap_err();
    assert_eq!(err.reason(), "push_limit");
    // Authorities: tunnel-only scopes count.
    assert_eq!(tunnel_only(2).check(PUSH, 2), Ok(()));
    let err = tunnel_only(3).check(PUSH, 2).unwrap_err();
    assert_eq!(err, Violation::PushLimit);
    let json = tunnel_only(3).to_json().unwrap();
    let err = Report::parse_checked(&json, PUSH, 2).unwrap_err();
    assert!(err.is_push_limit());
    assert!(Report::parse_checked(&json, PUSH, 3).is_ok());
    // Inbound scopes are not authorities.
    assert_eq!(v1_report().check(PUSH, 0), Ok(()));
}

#[derive(Debug, Clone)]
struct Ex {
    kind: u8,
    authority: usize,
    path: String,
    status: u16,
    tls: bool,
    sni: Option<usize>,
    dst: Option<usize>,
    tunnel: usize,
}

const AUTHORITIES: [&str; 5] = [
    "api.example.com:443",
    "[::1]:8080",
    "10.0.0.1:80",
    "other.example.com:80",
    "<redacted>:443",
];
const SNIS: [&str; 3] = ["Api.Example.com", "other.example.com.", "x_y.example.com"];
const DSTS: [&str; 3] = ["10.0.0.1:443", "[2001:db8::1]:8443", "[::ffff:10.0.0.9]:80"];

fn exchange() -> impl Strategy<Value = Ex> {
    (
        0u8..3,
        0..AUTHORITIES.len(),
        "(/[a-z0-9]{1,8}){1,4}",
        prop_oneof![Just(200u16), Just(404), 100u16..600],
        any::<bool>(),
        proptest::option::of(0..SNIS.len()),
        proptest::option::of(0..DSTS.len()),
        0..TUNNEL_KINDS.len(),
    )
        .prop_map(
            |(kind, authority, path, status, tls, sni, dst, tunnel)| Ex {
                kind,
                authority,
                path,
                status,
                tls,
                sni,
                dst,
                tunnel,
            },
        )
}

fn record_all(items: &[Ex], max_authorities: usize) -> Report {
    let mut l = limits();
    l.max_authorities = max_authorities;
    let mut r = Report::new(Some("p".into()));
    for e in items {
        let key = ScopeKey::parse(AUTHORITIES[e.authority]).expect("a key in the grammar");
        let direction = if e.kind == 0 {
            Direction::Inbound
        } else {
            Direction::Outbound
        };
        let mut ex = exchange_of(direction, &e.path);
        ex.status = e.status;
        ex.host = Some("svc");
        match e.kind {
            0 => {
                r.record(&l, &Redactor::default(), &ex);
            }
            1 => {
                let via = Via {
                    tls: e.tls,
                    sni: e.sni.map(|i| SNIS[i]),
                    dst: e.dst.map(|i| dst(DSTS[i])),
                };
                r.record_at(key, &via, &l, &Redactor::default(), &ex);
            }
            _ => {
                r.record_tunnel(&l, key, TUNNEL_KINDS[e.tunnel]);
            }
        }
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn shr_01_accepts_what_the_recorder_produces_with_outbound(
        items in proptest::collection::vec(exchange(), 1..8),
        split in 0usize..9,
        max_authorities in 1usize..6,
    ) {
        let split = split.min(items.len());
        let a = record_all(&items[..split], max_authorities);
        let b = record_all(&items[split..], max_authorities);
        for r in [&a, &b] {
            prop_assert_eq!(r.check(PUSH, AUTH), Ok(()), "recorded");
            let json = r.to_json().unwrap();
            prop_assert!(Report::parse_checked(&json, PUSH, AUTH).is_ok(), "round trip");
        }
        let mut l = limits();
        l.max_authorities = max_authorities;
        let merged = merge(a, b, &l);
        prop_assert_eq!(merged.check(PUSH, AUTH), Ok(()), "merged");
        prop_assert!(Report::parse_checked(&merged.to_json().unwrap(), PUSH, AUTH).is_ok());
    }
}
