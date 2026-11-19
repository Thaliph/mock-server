//! ADM-57: a target's answer to a load request, classified by `load::answer`.

use proptest::prelude::*;

use super::answer::{Answer, Expected, classify};
use super::summary::MimicError;
use crate::mesh::reason::Reason;

const TRUSTED: [&str; 3] = ["overloaded", "queue-timeout", "connect-failed"];

fn target() -> Expected<'static> {
    Expected {
        pod: "p-1",
        addr: "10.0.0.7:8080".parse().unwrap(),
    }
}

fn inbound(reason: &str) -> Answer {
    Answer::Inbound(MimicError::new(Reason::parse(reason.as_bytes()).unwrap()).unwrap())
}

/// The good refusal of `reason` by the target, as owned fields.
fn good(reason: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    [
        ("x-mimic-error", reason),
        ("x-mimic-origin", "inbound; pod=p-1"),
        ("x-mimic-endpoint", "10.0.0.7:8080"),
        ("content-length", "0"),
    ]
    .iter()
    .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
    .collect()
}

fn run(status: u16, fields: &[(Vec<u8>, Vec<u8>)], expected: &Expected<'_>) -> Answer {
    classify(
        status,
        fields.iter().map(|(n, v)| (n.as_slice(), v.as_slice())),
        expected,
    )
}

/// `fields` with the value of `name` replaced (removed when `None`).
fn with(fields: &[(Vec<u8>, Vec<u8>)], name: &str, value: Option<&str>) -> Vec<(Vec<u8>, Vec<u8>)> {
    fields
        .iter()
        .filter_map(|(n, v)| match (n == name.as_bytes(), value) {
            (false, _) => Some((n.clone(), v.clone())),
            (true, Some(value)) => Some((n.clone(), value.as_bytes().to_vec())),
            (true, None) => None,
        })
        .collect()
}

fn plus(fields: &[(Vec<u8>, Vec<u8>)], name: &str, value: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = fields.to_vec();
    out.push((name.as_bytes().to_vec(), value.to_vec()));
    out
}

#[test]
fn adm_57_a_bound_inbound_refusal_is_counted_under_its_reason() {
    for reason in TRUSTED {
        let fields = good(reason);
        assert_eq!(run(503, &fields, &target()), inbound(reason), "{reason}");
        let cases: [fn(&str) -> String; 2] = [str::to_ascii_uppercase, |s| {
            s.split('-')
                .map(|w| w[..1].to_ascii_uppercase() + &w[1..])
                .collect::<Vec<_>>()
                .join("-")
        }];
        for case in cases {
            let cased: Vec<_> = fields
                .iter()
                .map(|(n, v)| {
                    (
                        case(std::str::from_utf8(n).unwrap()).into_bytes(),
                        v.clone(),
                    )
                })
                .collect();
            assert_eq!(run(503, &cased, &target()), inbound(reason), "{reason}");
        }
        let mut more = plus(&fields, "date", b"Thu, 19 Nov 2026 13:00:00 GMT");
        more.insert(0, (b"server".to_vec(), b"x".to_vec()));
        assert_eq!(run(503, &more, &target()), inbound(reason), "{reason}");
    }
    let v6 = Expected {
        pod: "p-1",
        addr: "[fd00::7]:8080".parse().unwrap(),
    };
    let fields = with(
        &good("overloaded"),
        "x-mimic-endpoint",
        Some("[fd00::7]:8080"),
    );
    assert_eq!(run(503, &fields, &v6), inbound("overloaded"));
}

#[test]
fn adm_57_any_change_of_the_binding_is_unverified() {
    let base = good("overloaded");
    let mut cases: Vec<(String, u16, Vec<(Vec<u8>, Vec<u8>)>)> = Vec::new();
    for origin in [
        "inbound; pod=p-2",
        "inbound",
        "inbound; pod=p-1 ",
        "inbound;pod=p-1",
        "inbound; pod=p-10",
        "inbound; pod=P-1",
    ] {
        cases.push((
            origin.into(),
            503,
            with(&base, "x-mimic-origin", Some(origin)),
        ));
    }
    for endpoint in [
        "10.0.0.8:8080",
        "10.0.0.7:8081",
        "[fd00:0::7]:8080",
        "10.0.0.7",
        " 10.0.0.7:8080",
        "010.0.0.7:8080",
    ] {
        cases.push((
            endpoint.into(),
            503,
            with(&base, "x-mimic-endpoint", Some(endpoint)),
        ));
    }
    for name in ["x-mimic-error", "x-mimic-origin", "x-mimic-endpoint"] {
        cases.push((format!("no {name}"), 503, with(&base, name, None)));
        let value = base
            .iter()
            .find(|(n, _)| n == name.as_bytes())
            .unwrap()
            .1
            .clone();
        cases.push((format!("two {name}"), 503, plus(&base, name, &value)));
    }
    for status in [500, 502, 200, 504, 429] {
        cases.push((format!("status {status}"), status, base.clone()));
    }
    for reason in [
        "breaker-open",
        "no-endpoint",
        "Overloaded",
        "app-refused",
        "",
        "timeout",
        "overloaded ",
    ] {
        cases.push((
            format!("reason {reason:?}"),
            503,
            with(&base, "x-mimic-error", Some(reason)),
        ));
    }
    for (name, status, fields) in cases {
        assert_eq!(
            run(status, &fields, &target()),
            Answer::Unverified,
            "{name}"
        );
    }
    // An IPv6 target: only the display form of the address dialled is its endpoint.
    let v6 = Expected {
        pod: "p-1",
        addr: "[fd00::7]:8080".parse().unwrap(),
    };
    for endpoint in [
        "[fd00:0::7]:8080",
        "[FD00::7]:8080",
        "[fd00:0:0:0:0:0:0:7]:8080",
        "fd00::7:8080",
    ] {
        let fields = with(&base, "x-mimic-endpoint", Some(endpoint));
        assert_eq!(run(503, &fields, &v6), Answer::Unverified, "{endpoint}");
    }
}

#[test]
fn adm_57_an_answer_marked_ok_is_the_apps_whatever_else_it_carries() {
    let refusal = good("overloaded");
    assert_eq!(
        run(503, &plus(&refusal, "x-mimic-load", b"ok"), &target()),
        Answer::App
    );
    let mut first = refusal.clone();
    first.insert(0, (b"X-Mimic-Load".to_vec(), b"ok".to_vec()));
    assert_eq!(run(503, &first, &target()), Answer::App);
    for status in [200, 404] {
        let fields = vec![(b"x-mimic-load".to_vec(), b"ok".to_vec())];
        assert_eq!(run(status, &fields, &target()), Answer::App, "{status}");
    }
    let fields = vec![(b"x-mimic-load".to_vec(), b" ok ".to_vec())];
    assert_eq!(run(200, &fields, &target()), Answer::App);
}

#[test]
fn adm_57_load_marker_refusals_keep_their_outcomes() {
    let marker = |value: &[u8]| vec![(b"x-mimic-load".to_vec(), value.to_vec())];
    let mode = b"refused; reason=mode";
    assert_eq!(run(599, &marker(mode), &target()), Answer::ModeLeft);
    assert_eq!(
        run(
            503,
            &plus(&good("overloaded"), "x-mimic-load", mode),
            &target()
        ),
        Answer::ModeLeft
    );
    for value in [
        &b"refused; reason=workload"[..],
        b"refused; reason=ticket",
        b"refused",
        b"okay",
        b"o\x80k",
        b"refused; reason=mode\x80",
    ] {
        assert_eq!(
            run(599, &marker(value), &target()),
            Answer::Unverified,
            "{value:?}"
        );
    }
    // A marker other than `ok` is never read as absent, then trusted as a refusal.
    assert_eq!(
        run(
            503,
            &plus(&good("overloaded"), "x-mimic-load", b"\x80"),
            &target()
        ),
        Answer::Unverified
    );
    assert_eq!(
        run(
            503,
            &plus(&good("overloaded"), "x-mimic-load", b"okay"),
            &target()
        ),
        Answer::Unverified
    );
    // The first value counts, as `HeaderMap::get` gives it.
    let mut two = marker(b"refused; reason=ticket");
    two.push((b"x-mimic-load".to_vec(), b"ok".to_vec()));
    assert_eq!(run(200, &two, &target()), Answer::Unverified);
    assert_eq!(run(200, &[], &target()), Answer::Unverified);
}

/// One edit of the good refusal: (field index 0..3, name or value, kind, position, mask).
type Edit = (usize, bool, u8, prop::sample::Index, u8);

fn edited(edit: &Edit) -> (Vec<(Vec<u8>, Vec<u8>)>, bool) {
    let (field, name, kind, at, mask) = edit;
    let mut fields = good("queue-timeout");
    let target = if *name {
        &mut fields[*field].0
    } else {
        &mut fields[*field].1
    };
    let before = target.clone();
    match kind % 3 {
        0 => {
            let i = at.index(target.len());
            target[i] ^= (*mask).max(1);
        }
        1 => {
            let i = at.index(target.len() + 1);
            target.insert(i, *mask);
        }
        _ => {
            let i = at.index(target.len());
            target.remove(i);
        }
    }
    let case_only = *name && target.eq_ignore_ascii_case(&before) && target.len() == before.len();
    (fields, case_only)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn adm_57_a_mutated_refusal_is_never_trusted(
        edit in (
            0usize..3,
            any::<bool>(),
            any::<u8>(),
            any::<prop::sample::Index>(),
            prop_oneof![Just(0x20u8), any::<u8>()],
        ),
    ) {
        let (fields, case_only) = edited(&edit);
        let got = run(503, &fields, &target());
        if case_only {
            prop_assert_eq!(got, inbound("queue-timeout"));
        } else {
            prop_assert_eq!(got, Answer::Unverified);
        }
    }
}
