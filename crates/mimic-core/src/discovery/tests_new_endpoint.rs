//! MON-10 (unit): the recorder says whether a recorded exchange added an endpoint (spec §6, §9).

use super::observed::Observation;
use super::redact::Redactor;
use super::report::{Direction, Exchange, Limits, Outcome, Report, Via};
use super::scope::ScopeKey;

fn limits() -> Limits {
    Limits {
        max_literals: 3,
        max_endpoints: 256,
        max_authorities: 64,
        sample_budget: 1 << 20,
    }
}

fn exchange<'a>(direction: Direction, method: &'a str, path: &'a str, status: u16) -> Exchange<'a> {
    Exchange {
        direction,
        port: 8080,
        method,
        path,
        status,
        head_ms: 1,
        host: None,
        grpc: false,
        observation: Observation::default(),
    }
}

fn record(report: &mut Report, key: ScopeKey, e: &Exchange<'_>) -> Outcome {
    let via = Via {
        tls: false,
        sni: None,
        dst: None,
    };
    report.record_at(key, &via, &limits(), &Redactor::default(), e)
}

fn inbound(report: &mut Report, method: &str, path: &str, status: u16) -> Outcome {
    record(
        report,
        ScopeKey::Port(8080),
        &exchange(Direction::Inbound, method, path, status),
    )
}

fn added(outcome: Outcome) -> Option<bool> {
    match outcome {
        Outcome::Recorded { new_endpoint, .. } => Some(new_endpoint),
        _ => None,
    }
}

#[test]
fn mon_10_a_recorded_exchange_tells_whether_it_added_an_endpoint() {
    let mut report = Report::new(None);
    assert_eq!(added(inbound(&mut report, "GET", "/a", 200)), Some(true));
    assert_eq!(added(inbound(&mut report, "GET", "/a", 200)), Some(false));
    assert_eq!(added(inbound(&mut report, "POST", "/a", 200)), Some(true));
}

#[test]
fn mon_10_the_exchange_whose_literal_makes_a_template_by_absorption_is_new() {
    let mut report = Report::new(None);
    for path in ["/u/ann", "/u/bob", "/u/cat"] {
        assert_eq!(
            added(inbound(&mut report, "GET", path, 200)),
            Some(true),
            "{path}"
        );
    }
    // The fourth literal is past `max_literals`: the node absorbs them into `/u/{u_id}`.
    assert_eq!(
        added(inbound(&mut report, "GET", "/u/dan", 200)),
        Some(true)
    );
    assert!(
        report
            .endpoints()
            .iter()
            .any(|e| e.template == "/u/{u_id}" && e.method == "GET")
    );
    // A literal the template covers adds nothing.
    assert_eq!(
        added(inbound(&mut report, "GET", "/u/eve", 200)),
        Some(false)
    );
}

#[test]
fn mon_10_an_exchange_that_is_not_recorded_is_not_new() {
    let mut report = Report::new(None);
    assert_eq!(
        inbound(&mut report, "GET", "/missing", 404),
        Outcome::NotFound
    );
    assert_eq!(inbound(&mut report, "GET", "/a", 99), Outcome::Invalid);
    assert!(added(inbound(&mut report, "GET", "/missing", 404)).is_none());
}

#[test]
fn mon_10_outbound_the_same_path_under_a_new_authority_scope_is_new() {
    let mut report = Report::new(None);
    let call = |report: &mut Report, authority: &str| {
        record(
            report,
            ScopeKey::Authority(authority.into()),
            &exchange(Direction::Outbound, "GET", "/v1/items", 200),
        )
    };
    assert_eq!(added(call(&mut report, "a.example:443")), Some(true));
    assert_eq!(added(call(&mut report, "a.example:443")), Some(false));
    assert_eq!(added(call(&mut report, "b.example:443")), Some(true));
}
