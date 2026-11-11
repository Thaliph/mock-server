//! EGR-02, EGR-04, EGR-05, EGR-08: outbound recording semantics (spec §6.1).

use std::net::SocketAddr;

use proptest::prelude::*;
use serde_json::Value;

use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Outcome, Report, Sampled, Tunneled, Via, merge};
use super::scope::ScopeKey;

fn limits() -> Limits {
    Limits {
        max_literals: 8,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 16 << 20,
    }
}

fn ex<'a>(direction: Direction, path: &'a str, status: u16) -> Exchange<'a> {
    Exchange {
        direction,
        port: 8080,
        method: "GET",
        path,
        status,
        head_ms: 5,
        host: None,
        grpc: false,
        observation: Observation::default(),
    }
}

fn auth(a: &str) -> ScopeKey {
    ScopeKey::Authority(a.to_string())
}

const PLAIN: Via<'static> = Via {
    tls: false,
    sni: None,
    dst: None,
};

fn out(r: &mut Report, l: &Limits, authority: &str, path: &str, status: u16) -> Outcome {
    r.record_at(
        auth(authority),
        &PLAIN,
        l,
        &Redactor::default(),
        &ex(Direction::Outbound, path, status),
    )
}

fn inb(r: &mut Report, l: &Limits, path: &str, status: u16) -> Outcome {
    r.record(
        l,
        &Redactor::default(),
        &ex(Direction::Inbound, path, status),
    )
}

fn json(r: &Report) -> Value {
    serde_json::from_str(&r.to_json().unwrap()).unwrap()
}

fn outbound_endpoints(r: &Report) -> Vec<(String, String)> {
    r.endpoints()
        .into_iter()
        .filter(|e| e.direction == Direction::Outbound)
        .map(|e| (e.authority.unwrap().to_string(), e.template))
        .collect()
}

#[test]
fn egr_02_endpoints_are_per_authority() {
    let l = Limits {
        max_literals: 2,
        ..limits()
    };
    let mut r = Report::new(None);
    for a in ["a.example:443", "b.example:443"] {
        out(&mut r, &l, a, "/v1/payments/42", 200);
    }
    assert_eq!(
        outbound_endpoints(&r),
        [
            (
                "a.example:443".to_string(),
                "/v1/payments/{payments_id}".to_string()
            ),
            (
                "b.example:443".to_string(),
                "/v1/payments/{payments_id}".to_string()
            ),
        ]
    );
    // Absorption at one authority leaves the other untouched.
    for p in ["/u/aa", "/u/bb", "/u/cc"] {
        out(&mut r, &l, "a.example:443", p, 200);
    }
    out(&mut r, &l, "b.example:443", "/u/aa", 200);
    let found = outbound_endpoints(&r);
    assert!(
        found.contains(&("a.example:443".into(), "/u/{u_id}".into())),
        "{found:?}"
    );
    assert!(
        !found
            .iter()
            .any(|(a, t)| a == "a.example:443" && t == "/u/aa"),
        "{found:?}"
    );
    assert!(
        found.contains(&("b.example:443".into(), "/u/aa".into())),
        "{found:?}"
    );
}

#[test]
fn egr_02_an_outbound_endpoint_has_no_hosts() {
    let mut r = Report::new(None);
    let mut e = ex(Direction::Outbound, "/x", 200);
    e.host = Some("api.example.com");
    r.record_at(
        auth("api.example.com:80"),
        &PLAIN,
        &limits(),
        &Redactor::default(),
        &e,
    );
    let e = r.endpoints().remove(0);
    assert!(e.stats.hosts.is_empty());
}

#[test]
fn egr_02_origin_counts_schemes_and_bounds_sni_and_dst() {
    let l = limits();
    let mut r = Report::new(None);
    let plain = |r: &mut Report| out(r, &l, "api.example.com:443", "/x", 200);
    plain(&mut r);
    plain(&mut r);
    for i in (0..20).rev() {
        let sni = format!("h{i:02}.example.com");
        let dst: SocketAddr = format!("10.0.0.{i}:443").parse().unwrap();
        r.record_at(
            auth("api.example.com:443"),
            &Via {
                tls: true,
                sni: Some(&sni),
                dst: Some(dst),
            },
            &l,
            &Redactor::default(),
            &ex(Direction::Outbound, "/x", 200),
        );
    }
    let v = json(&r);
    let origin = &v["scopes"]["outbound"]["api.example.com:443"]["origin"];
    assert_eq!(
        origin["schemes"],
        serde_json::json!({"http": 2, "https": 20})
    );
    let sni: Vec<&str> = origin["sni"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    let dst: Vec<&str> = origin["dst"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(sni.len(), 16);
    assert_eq!(sni[0], "h00.example.com");
    assert_eq!(sni[15], "h15.example.com");
    assert_eq!(dst.len(), 16);
    let mut sorted = dst.clone();
    sorted.sort_unstable();
    assert_eq!(dst, sorted);
    assert_eq!(dst[0], "10.0.0.0:443");
}

#[test]
fn egr_02_outbound_404_and_405_are_recorded_inbound_404_is_not() {
    let l = limits();
    let mut r = Report::new(None);
    assert_eq!(
        out(&mut r, &l, "api.example.com:443", "/nope", 404),
        Outcome::Recorded {
            new_endpoint: true,
            sample: Sampled::Admitted
        }
    );
    let e = &r.endpoints()[0];
    assert_eq!(e.stats.samples().len(), 1);
    assert_eq!(
        out(&mut r, &l, "api.example.com:443", "/nope", 405),
        Outcome::Recorded {
            new_endpoint: false,
            sample: Sampled::Admitted
        }
    );
    assert_eq!(
        out(&mut r, &l, "api.example.com:443", "/other", 405),
        Outcome::Recorded {
            new_endpoint: true,
            sample: Sampled::Admitted
        }
    );
    assert_eq!(inb(&mut r, &l, "/nope", 404), Outcome::NotFound);
    assert_eq!(inb(&mut r, &l, "/nope", 405), Outcome::NotFound);
}

#[test]
fn egr_05_the_authority_limit_refuses_a_third_scope() {
    let l = Limits {
        max_authorities: 2,
        ..limits()
    };
    let mut r = Report::new(None);
    out(&mut r, &l, "a.example:443", "/x", 200);
    out(&mut r, &l, "b.example:443", "/x", 200);
    assert_eq!(
        out(&mut r, &l, "c.example:443", "/x", 200),
        Outcome::AuthorityLimit
    );
    assert_eq!(
        r.record_tunnel(&l, auth("c.example:443"), "tcp"),
        Tunneled::AuthorityLimit
    );
    // Known authorities still update, by exchange and by tunnel.
    assert!(matches!(
        out(&mut r, &l, "a.example:443", "/y", 200),
        Outcome::Recorded { .. }
    ));
    assert_eq!(
        r.record_tunnel(&l, auth("b.example:443"), "tcp"),
        Tunneled::Counted
    );
    assert_eq!(json(&r)["scopes"]["outbound"].as_object().unwrap().len(), 2);
}

#[test]
fn egr_05_the_endpoint_limit_is_per_direction() {
    let l = Limits {
        max_endpoints: 3,
        ..limits()
    };
    let mut r = Report::new(None);
    for p in ["/a", "/b", "/c"] {
        out(&mut r, &l, "a.example:443", p, 200);
    }
    assert_eq!(
        out(&mut r, &l, "a.example:443", "/d", 200),
        Outcome::EndpointLimit
    );
    assert!(matches!(
        inb(&mut r, &l, "/a", 200),
        Outcome::Recorded {
            new_endpoint: true,
            ..
        }
    ));
    let mut r = Report::new(None);
    for p in ["/a", "/b", "/c"] {
        inb(&mut r, &l, p, 200);
    }
    assert_eq!(inb(&mut r, &l, "/d", 200), Outcome::EndpointLimit);
    assert!(matches!(
        out(&mut r, &l, "a.example:443", "/a", 200),
        Outcome::Recorded {
            new_endpoint: true,
            ..
        }
    ));
}

/// A request body that makes a sample of about `n` bytes.
fn big(n: usize) -> Vec<u8> {
    let mut b = br#"{"v":""#.to_vec();
    b.extend(std::iter::repeat_n(b'x', n));
    b.extend_from_slice(br#""}"#);
    b
}

#[test]
fn egr_05_outbound_samples_stop_at_half_the_budget() {
    let body = big(2000);
    let headers = [("content-type", "application/json")];
    let obs = Observation {
        req_headers: &headers,
        req_body: Some(&body),
        ..Observation::default()
    };
    fn probe<'a>(obs: &Observation<'a>, direction: Direction, path: &'a str) -> Exchange<'a> {
        Exchange {
            observation: obs.clone(),
            ..ex(direction, path, 200)
        }
    }
    // One sample's size.
    let mut one = Report::new(None);
    one.record(
        &limits(),
        &Redactor::default(),
        &probe(&obs, Direction::Inbound, "/p"),
    );
    let size = one.sample_bytes();
    assert!(size > 2000);
    let l = Limits {
        sample_budget: size * 4 + size / 2,
        ..limits()
    };
    let mut r = Report::new(None);
    let mut sampled = Vec::new();
    for i in 0..4 {
        let path = format!("/o{i}");
        let o = r.record_at(
            auth("a.example:443"),
            &PLAIN,
            &l,
            &Redactor::default(),
            &probe(&obs, Direction::Outbound, &path),
        );
        sampled.push(o);
    }
    let admitted = sampled
        .iter()
        .filter(|o| {
            matches!(
                o,
                Outcome::Recorded {
                    sample: Sampled::Admitted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(admitted, 2, "{sampled:?}");
    assert!(sampled.iter().any(|o| matches!(
        o,
        Outcome::Recorded {
            sample: Sampled::Budget,
            ..
        }
    )));
    // Inbound still uses the rest of the whole budget.
    let mut inbound = 0;
    for i in 0..4 {
        let path = format!("/i{i}");
        if matches!(
            r.record(
                &l,
                &Redactor::default(),
                &probe(&obs, Direction::Inbound, &path)
            ),
            Outcome::Recorded {
                sample: Sampled::Admitted,
                ..
            }
        ) {
            inbound += 1;
        }
    }
    assert_eq!(inbound, 2);
    assert!(r.sample_bytes() <= l.sample_budget);
    // The merge enforces the same split.
    let merged = merge(
        Report::new(None),
        r.clone(),
        &Limits {
            sample_budget: size * 2,
            ..l
        },
    );
    assert!(merged.sample_bytes() <= size * 2);
}

#[test]
fn egr_08_tunnels_are_counted_by_kind_without_endpoint_sample_or_hosts() {
    let l = limits();
    let mut r = Report::new(None);
    let kinds = [
        "tcp",
        "connect",
        "tls_excluded",
        "tls_no_ca",
        "tls_unsupported",
        "tls_upstream_untrusted",
        "tls_client_cert_requested",
        "tls_client_rejected",
    ];
    for k in kinds {
        assert_eq!(
            r.record_tunnel(&l, auth("db.example:5432"), k),
            Tunneled::Counted
        );
    }
    assert_eq!(
        r.record_tunnel(&l, auth("db.example:5432"), "tcp"),
        Tunneled::Counted
    );
    assert_eq!(
        r.record_tunnel(&l, auth("10.0.0.5:9000"), "tcp"),
        Tunneled::Counted
    );
    assert_eq!(
        r.record_tunnel(&l, auth("db.example:5432"), "bogus"),
        Tunneled::Invalid
    );
    assert!(r.endpoints().is_empty());
    assert_eq!(r.sample_bytes(), 0);
    assert_eq!(r.exchanges(), 0);
    let v = json(&r);
    assert_eq!(v["version"], 2);
    let t = &v["scopes"]["outbound"]["db.example:5432"]["tunnels"];
    assert_eq!(t["tcp"], 2);
    assert_eq!(t["tls_no_ca"], 1);
    assert_eq!(t.as_object().unwrap().len(), 8);
    assert_eq!(
        v["scopes"]["outbound"]["10.0.0.5:9000"]["tunnels"]["tcp"],
        1
    );
    assert!(!v.to_string().contains("hosts"));
    // Merges add the counts and the report round-trips.
    let twice = merge(r.clone(), r.clone(), &l);
    assert_eq!(
        json(&twice)["scopes"]["outbound"]["db.example:5432"]["tunnels"]["tcp"],
        4
    );
    assert_eq!(Report::from_json(&r.to_json().unwrap()).unwrap(), r);
}

#[test]
fn egr_08_a_scope_holds_tunnels_and_endpoints_together() {
    let l = limits();
    let mut r = Report::new(None);
    out(&mut r, &l, "a.example:443", "/x", 200);
    r.record_tunnel(&l, auth("a.example:443"), "tls_no_ca");
    let v = json(&r);
    let scope = &v["scopes"]["outbound"]["a.example:443"];
    assert_eq!(scope["tunnels"]["tls_no_ca"], 1);
    assert_eq!(outbound_endpoints(&r).len(), 1);
}

// ---- EGR-04, EGR-05: properties ----

#[derive(Debug, Clone)]
enum Op {
    In(u8, u16),
    Out(u8, u8, u16, bool, u8),
    Tunnel(u8, u8),
}

const KINDS: [&str; 4] = ["tcp", "connect", "tls_no_ca", "tls_excluded"];

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Inbound `404` is state dependent (`not_found` on an unknown endpoint, DSC-11): the
        // partition property needs exchanges that do not depend on what came before.
        (0u8..4, prop_oneof![Just(200u16), Just(500)]).prop_map(|(p, s)| Op::In(p, s)),
        (
            0u8..4,
            0u8..4,
            prop_oneof![Just(200u16), Just(404), Just(405), Just(503)],
            any::<bool>(),
            0u8..20
        )
            .prop_map(|(a, p, s, tls, n)| Op::Out(a, p, s, tls, n)),
        (0u8..4, 0u8..4).prop_map(|(a, k)| Op::Tunnel(a, k)),
    ]
}

fn apply(r: &mut Report, l: &Limits, op: &Op) {
    let red = Redactor::default();
    match op {
        Op::In(p, s) => {
            r.record(l, &red, &ex(Direction::Inbound, &format!("/i{p}"), *s));
        }
        Op::Out(a, p, s, tls, n) => {
            let sni = format!("s{n:02}.example");
            let dst: SocketAddr = format!("10.0.0.{n}:443").parse().unwrap();
            let via = Via {
                tls: *tls,
                sni: tls.then_some(sni.as_str()),
                dst: Some(dst),
            };
            r.record_at(
                auth(&format!("h{a}.example:443")),
                &via,
                l,
                &red,
                &ex(Direction::Outbound, &format!("/o{p}"), *s),
            );
        }
        Op::Tunnel(a, k) => {
            r.record_tunnel(
                l,
                auth(&format!("h{a}.example:443")),
                KINDS[usize::from(*k)],
            );
        }
    }
}

fn built(l: &Limits, ops: &[Op]) -> Report {
    let mut r = Report::new(Some("p".into()));
    for o in ops {
        apply(&mut r, l, o);
    }
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn egr_04_partitions_merge_to_the_sequence(
        ops in proptest::collection::vec(op(), 0..40),
        parts in proptest::collection::vec(0usize..3, 40),
    ) {
        let l = limits();
        let whole = built(&l, &ops);
        let mut groups: [Vec<Op>; 3] = Default::default();
        for (o, p) in ops.iter().zip(&parts) {
            groups[*p].push(o.clone());
        }
        let merged = groups
            .iter()
            .map(|g| built(&l, g))
            .fold(Report::new(Some("p".into())), |acc, r| merge(acc, r, &l));
        prop_assert_eq!(whole.to_json().unwrap(), merged.to_json().unwrap());
    }

    #[test]
    fn egr_04_merge_is_commutative_and_associative_under_the_limits(
        a in proptest::collection::vec(op(), 0..25),
        b in proptest::collection::vec(op(), 0..25),
        c in proptest::collection::vec(op(), 0..25),
        max_authorities in 1usize..4,
        max_endpoints in 1usize..6,
    ) {
        let wide = limits();
        let l = Limits { max_authorities, max_endpoints, ..limits() };
        let (a, b, c) = (built(&wide, &a), built(&wide, &b), built(&wide, &c));
        let ab = merge(a.clone(), b.clone(), &l);
        let ba = merge(b.clone(), a.clone(), &l);
        prop_assert_eq!(&ab, &ba);
        let left = merge(ab, c.clone(), &l);
        let right = merge(a, merge(b, c, &l), &l);
        prop_assert_eq!(&left, &right);
        let held = json(&left)["scopes"]["outbound"].as_object().map_or(0, |m| m.len());
        prop_assert!(held <= max_authorities);
        for d in [Direction::Inbound, Direction::Outbound] {
            prop_assert!(left.endpoints().iter().filter(|e| e.direction == d).count() <= max_endpoints);
        }
    }

    #[test]
    fn egr_05_the_recorder_never_panics_and_state_stays_bounded(
        ops in proptest::collection::vec(op(), 0..60),
    ) {
        let l = Limits { max_literals: 4, max_endpoints: 3, max_authorities: 2, sample_budget: 1 << 20 };
        let r = built(&l, &ops);
        let held = json(&r)["scopes"]["outbound"].as_object().map_or(0, |m| m.len());
        prop_assert!(held <= l.max_authorities);
        for d in [Direction::Inbound, Direction::Outbound] {
            prop_assert!(r.endpoints().iter().filter(|e| e.direction == d).count() <= l.max_endpoints);
        }
        prop_assert!(r.sample_bytes() <= l.sample_budget);
        let retained = r.to_json().unwrap().len() - r.sample_bytes();
        prop_assert!(retained <= Report::state_bound(&l), "{retained} > {}", Report::state_bound(&l));
    }
}

#[test]
fn egr_02_origin_dst_writes_port_zero_as_one_like_the_scope_key() {
    let l = limits();
    let mut r = Report::new(None);
    for dst in ["10.0.0.1:0", "[2001:db8::1]:0", "10.0.0.2:7"] {
        r.record_at(
            auth("api.example.com:443"),
            &Via {
                tls: false,
                sni: None,
                dst: Some(dst.parse().unwrap()),
            },
            &l,
            &Redactor::default(),
            &ex(Direction::Outbound, "/x", 200),
        );
    }
    let v = json(&r);
    let dst = &v["scopes"]["outbound"]["api.example.com:443"]["origin"]["dst"];
    assert_eq!(
        *dst,
        serde_json::json!(["10.0.0.1:1", "10.0.0.2:7", "[2001:db8::1]:1"])
    );
    assert_eq!(Report::from_json(&r.to_json().unwrap()).unwrap(), r);
}

/// The fuzz invariant (PUL-20): a projected set reads back byte for byte, also when the report
/// still holds the legacy marker of up to F12 (the projection is the upgraded form).
#[test]
fn plh_07_a_projection_of_a_legacy_report_reads_back_byte_for_byte() {
    use super::egress::EgressSet;
    let path = format!(
        "{}/tests/fixtures/f12-golden/single-below.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let report = Report::from_json(&std::fs::read_to_string(path).expect("fixture")).expect("ok");
    assert!(report.to_json().expect("json").contains("<redacted>"));
    let set = EgressSet::project(&report);
    let json = set.to_json().expect("serialises");
    let back = EgressSet::from_json(&json).expect("accepted");
    assert_eq!(back.to_json().expect("serialises"), json);
    // Only the authority hosts keep the marker.
    assert!(!json.replace("<redacted>:", "").contains("<redacted>"));
}
