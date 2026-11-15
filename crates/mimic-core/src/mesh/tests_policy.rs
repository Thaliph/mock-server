//! MSH-07 and MSH-08: the policy of `MimicConfig`s (spec §7).

use super::input::*;
use super::policy::*;
use super::testkit::*;

fn slow(ns: &str, name: &str, created: i64, svc: &str, secs: Option<u32>) -> Config {
    Config {
        slow_start: secs,
        ..cfg(ns, name, created, &[(TargetKind::Service, svc)])
    }
}

fn slow_start(s: &Snapshot, ns: &str, d: &str, key: &str) -> u32 {
    view_of(s, ns, d).view.services[key]
        .policy
        .load_balancer
        .slow_start_seconds
}

fn cluster() -> Snapshot {
    let mut s = snapshot();
    s.services = vec![
        http_svc("shop", "cart", "10.96.0.1"),
        http_svc("risk", "scoring", "10.50.0.1"),
    ];
    s.deployments = vec![("shop".into(), "cart".into())];
    s
}

fn status(s: &Snapshot, ns: &str, name: &str) -> ConfigStatus {
    statuses(s)[&(ns.to_string(), name.to_string())].clone()
}

#[test]
fn msh_07_the_slow_start_of_the_winning_config_is_in_the_view_and_the_default_is_30() {
    let mut s = cluster();
    assert_eq!(defaults().load_balancer.slow_start_seconds, 30);
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 30);
    s.configs = vec![slow("shop", "a", 1, "cart", Some(45))];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 45);
    s.configs = vec![slow("shop", "a", 1, "cart", Some(0))];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 0);
    s.configs = vec![slow("shop", "a", 1, "cart", None)];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 30);
}

#[test]
fn msh_07_a_consented_service_carries_the_policy_of_its_own_namespace() {
    let mut s = cluster();
    s.grants = Some(vec![grant("risk", "shop")]);
    s.configs = vec![
        dep_cfg("shop", "d", 1, "cart", &["risk/scoring"]),
        slow("risk", "p", 1, "scoring", Some(7)),
        slow("shop", "q", 1, "scoring", Some(99)), // targets shop/scoring, which does not exist
    ];
    assert_eq!(slow_start(&s, "shop", "cart", "risk/scoring"), 7);
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 30);
}

#[test]
fn msh_08_the_oldest_config_wins_as_a_whole_and_the_others_conflict() {
    let mut s = cluster();
    s.configs = vec![
        slow("shop", "b", 5, "cart", Some(10)),
        slow("shop", "a", 9, "cart", Some(20)),
        slow("shop", "c", 5, "cart", Some(30)),
    ];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 10); // b and c tie on age: the name decides
    assert!(status(&s, "shop", "b").accepted.ok);
    for loser in ["a", "c"] {
        let a = status(&s, "shop", loser).accepted;
        assert!(
            !a.ok && a.reason == "Conflicted" && a.message.contains("shop/b"),
            "{loser}"
        );
    }
}

#[test]
fn msh_08_a_config_can_win_one_target_and_lose_another() {
    let mut s = cluster();
    s.services.push(http_svc("shop", "db", "10.96.0.2"));
    s.configs = vec![
        Config {
            slow_start: Some(11),
            ..cfg(
                "shop",
                "x",
                5,
                &[(TargetKind::Service, "cart"), (TargetKind::Service, "db")],
            )
        },
        slow("shop", "y", 1, "cart", Some(22)),
    ];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 22);
    assert_eq!(slow_start(&s, "shop", "cart", "shop/db"), 11);
    assert_eq!(status(&s, "shop", "x").accepted.reason, "Conflicted");
    assert!(status(&s, "shop", "y").accepted.ok);
}

#[test]
fn msh_08_an_invalid_config_takes_no_part_in_the_precedence() {
    let mut s = cluster();
    let too_slow = slow("shop", "old", 1, "cart", Some(MAX_SLOW_START_SECS + 1));
    s.configs = vec![too_slow, slow("shop", "new", 9, "cart", Some(12))];
    assert_eq!(slow_start(&s, "shop", "cart", "shop/cart"), 12);
    assert_eq!(status(&s, "shop", "old").accepted.reason, "Invalid");
    assert!(status(&s, "shop", "new").accepted.ok);
}

fn invalid(c: &Config) -> bool {
    validate(c).is_err()
}

#[test]
fn msh_08_the_rules_the_cel_states_are_checked_again() {
    let dep = |n: &str| (TargetKind::Deployment, n.to_string());
    let svc = |n: &str| (TargetKind::Service, n.to_string());
    let make = |targets: Vec<(TargetKind, String)>| {
        let refs: Vec<(TargetKind, &str)> = targets.iter().map(|(k, n)| (*k, n.as_str())).collect();
        cfg("shop", "x", 1, &refs)
    };
    let d = |n: usize| {
        Some(
            (0..n)
                .map(|i| Destination {
                    namespace: "risk".into(),
                    name: format!("s{i}"),
                })
                .collect::<Vec<_>>(),
        )
    };
    assert!(!invalid(&make(vec![dep("cart"), svc("cart")])));
    assert!(invalid(&make(vec![])), "no target");
    assert!(
        invalid(&make((0..17).map(|i| svc(&format!("s{i}"))).collect())),
        "17 targets"
    );
    assert!(!invalid(&make(
        (0..16).map(|i| svc(&format!("s{i}"))).collect()
    )));
    assert!(invalid(&make(vec![dep("")])), "empty target name");
    assert!(!invalid(&Config {
        destinations: d(256),
        ..make(vec![dep("cart")])
    }));
    assert!(
        invalid(&Config {
            destinations: d(257),
            ..make(vec![dep("cart")])
        }),
        "257 destinations"
    );
    let mut c = Config {
        destinations: d(1),
        ..make(vec![dep("cart")])
    };
    c.destinations.as_mut().unwrap()[0].name.clear();
    assert!(invalid(&c), "destination without a name");
    c.destinations.as_mut().unwrap()[0].name = "x".into();
    c.destinations.as_mut().unwrap()[0].namespace.clear();
    assert!(invalid(&c), "destination without a namespace");
    assert!(
        invalid(&Config {
            destinations: d(1),
            ..make(vec![svc("cart")])
        }),
        "destinations on a Service"
    );
    assert!(
        invalid(&Config {
            mesh_enabled: Some(false),
            ..make(vec![svc("cart")])
        }),
        "mesh on a Service"
    );
    assert!(
        invalid(&Config {
            slow_start: Some(5),
            ..make(vec![dep("cart")])
        }),
        "loadBalancer on a Deployment"
    );
    assert!(!invalid(&Config {
        slow_start: Some(MAX_SLOW_START_SECS),
        ..make(vec![svc("cart")])
    }));
    assert!(invalid(&Config {
        slow_start: Some(MAX_SLOW_START_SECS + 1),
        ..make(vec![svc("cart")])
    }));
}

#[test]
fn msh_08_a_missing_target_is_named_and_reasons_have_an_order() {
    let mut s = cluster();
    s.configs = vec![
        cfg(
            "shop",
            "gone",
            1,
            &[
                (TargetKind::Deployment, "nope"),
                (TargetKind::Service, "cart"),
            ],
        ),
        cfg("shop", "ok", 1, &[(TargetKind::Deployment, "cart")]),
        // Invalid and missing target: Invalid first.
        slow("shop", "both", 1, "nope", Some(MAX_SLOW_START_SECS + 1)),
        // Missing target and a loser: TargetNotFound first.
        cfg(
            "shop",
            "lost",
            9,
            &[
                (TargetKind::Deployment, "cart"),
                (TargetKind::Service, "nope"),
            ],
        ),
    ];
    let a = status(&s, "shop", "gone").accepted;
    assert!(
        !a.ok
            && a.reason == "TargetNotFound"
            && a.message.contains("Deployment nope")
            && !a.message.contains("Service cart")
    );
    let ok = status(&s, "shop", "ok");
    assert!(
        ok.accepted.ok
            && ok.accepted.reason == "Accepted"
            && ok.resolved_refs.ok
            && ok.resolved_refs.reason == "ResolvedRefs"
    );
    assert_eq!(status(&s, "shop", "both").accepted.reason, "Invalid");
    assert_eq!(status(&s, "shop", "lost").accepted.reason, "TargetNotFound");
}

#[test]
fn msh_08_a_target_is_always_in_the_namespace_of_its_config() {
    // `risk/scoring` exists, but a config of `shop` naming `scoring` targets `shop/scoring`.
    let mut s = cluster();
    s.configs = vec![slow("shop", "x", 1, "scoring", Some(5))];
    assert_eq!(status(&s, "shop", "x").accepted.reason, "TargetNotFound");
    assert_eq!(slow_start(&s, "risk", "other", "risk/scoring"), 30);
}

fn retry_cfg(
    ns: &str,
    name: &str,
    created: i64,
    svc: &str,
    retries: Option<u32>,
    budget: Option<RetryBudgetSpec>,
) -> Config {
    Config {
        retries,
        retry_budget: budget,
        ..cfg(ns, name, created, &[(TargetKind::Service, svc)])
    }
}

fn budget(
    percent: Option<u32>,
    min_per_second: Option<u32>,
    window_seconds: Option<u32>,
) -> RetryBudgetSpec {
    RetryBudgetSpec {
        percent,
        min_per_second,
        window_seconds,
    }
}

fn retry_policy(s: &Snapshot, ns: &str, d: &str, key: &str) -> (u32, RetryBudget) {
    let p = &view_of(s, ns, d).view.services[key].policy;
    (p.retries, p.retry_budget.clone())
}

fn rb(percent: u32, min_per_second: u32, window_seconds: u32) -> RetryBudget {
    RetryBudget {
        percent,
        min_per_second,
        window_seconds,
    }
}

#[test]
fn rty_44_the_defaults_are_3_retries_and_a_budget_of_20_percent_10_per_second_10_s() {
    let mut s = cluster();
    assert_eq!(defaults().retries, 3);
    assert_eq!(defaults().retry_budget, rb(20, 10, 10));
    assert_eq!(effective(None), defaults());
    assert_eq!(
        retry_policy(&s, "shop", "cart", "shop/cart"),
        (3, rb(20, 10, 10))
    );
    s.configs = vec![retry_cfg("shop", "a", 1, "cart", None, None)];
    assert_eq!(
        retry_policy(&s, "shop", "cart", "shop/cart"),
        (3, rb(20, 10, 10))
    );
}

#[test]
fn rty_44_the_winning_config_sets_retries_and_each_budget_field_alone() {
    let mut s = cluster();
    s.configs = vec![retry_cfg("shop", "a", 1, "cart", Some(0), None)];
    assert_eq!(retry_policy(&s, "shop", "cart", "shop/cart").0, 0);
    s.configs = vec![retry_cfg("shop", "a", 1, "cart", Some(10), None)];
    assert_eq!(retry_policy(&s, "shop", "cart", "shop/cart").0, 10);
    for (set, want) in [
        (budget(Some(0), None, None), rb(0, 10, 10)),
        (budget(None, Some(0), None), rb(20, 0, 10)),
        (budget(None, None, Some(60)), rb(20, 10, 60)),
        (budget(Some(100), Some(1000), Some(1)), rb(100, 1000, 1)),
        (budget(None, None, None), rb(20, 10, 10)),
    ] {
        s.configs = vec![retry_cfg("shop", "a", 1, "cart", None, Some(set))];
        assert_eq!(
            retry_policy(&s, "shop", "cart", "shop/cart"),
            (3, want),
            "an unset field keeps its default alone"
        );
    }
}

#[test]
fn rty_44_a_consented_service_carries_the_retries_of_its_own_namespace() {
    let mut s = cluster();
    s.grants = Some(vec![grant("risk", "shop")]);
    s.configs = vec![
        dep_cfg("shop", "d", 1, "cart", &["risk/scoring"]),
        retry_cfg(
            "risk",
            "p",
            1,
            "scoring",
            Some(7),
            Some(budget(Some(5), None, None)),
        ),
        retry_cfg("shop", "q", 1, "scoring", Some(9), None), // shop/scoring does not exist
    ];
    assert_eq!(
        retry_policy(&s, "shop", "cart", "risk/scoring"),
        (7, rb(5, 10, 10))
    );
    assert_eq!(
        retry_policy(&s, "shop", "cart", "shop/cart"),
        (3, rb(20, 10, 10))
    );
}

#[test]
fn rty_44_validate_refuses_retries_out_of_bounds_and_on_a_deployment() {
    let on = |kind, retries, retry_budget| Config {
        retries,
        retry_budget,
        ..cfg("shop", "x", 1, &[(kind, "cart")])
    };
    let svc = |retries, b| on(TargetKind::Service, retries, b);
    assert!(validate(&svc(Some(0), None)).is_ok());
    assert!(validate(&svc(Some(10), None)).is_ok());
    assert!(validate(&svc(Some(11), None)).is_err());
    assert!(validate(&svc(None, Some(budget(Some(100), Some(1000), Some(60))))).is_ok());
    assert!(validate(&svc(None, Some(budget(Some(0), Some(0), Some(1))))).is_ok());
    assert!(validate(&svc(None, Some(budget(None, None, None)))).is_ok());
    for bad in [
        budget(Some(101), None, None),
        budget(None, Some(1001), None),
        budget(None, None, Some(0)),
        budget(None, None, Some(61)),
    ] {
        let err = validate(&svc(None, Some(bad))).expect_err("out of bounds");
        assert!(err.contains("retryBudget"), "{err}");
    }
    let err = validate(&svc(Some(11), None)).unwrap_err();
    assert!(err.contains("retries"), "{err}");
    let dep = |retries, b| on(TargetKind::Deployment, retries, b);
    assert!(
        validate(&dep(Some(3), None))
            .unwrap_err()
            .contains("retries")
    );
    assert!(
        validate(&dep(None, Some(budget(None, None, None))))
            .unwrap_err()
            .contains("retryBudget")
    );
    // A Deployment target next to a Service one still fits.
    let both = Config {
        retries: Some(2),
        ..cfg(
            "shop",
            "x",
            1,
            &[
                (TargetKind::Deployment, "cart"),
                (TargetKind::Service, "cart"),
            ],
        )
    };
    assert!(validate(&both).is_ok());
}

#[test]
fn rty_44_an_invalid_retries_config_is_invalid_in_the_status_and_takes_no_part() {
    let mut s = cluster();
    s.configs = vec![
        retry_cfg("shop", "a", 1, "cart", Some(11), None),
        retry_cfg("shop", "b", 2, "cart", Some(2), None),
    ];
    assert_eq!(status(&s, "shop", "a").accepted.reason, "Invalid");
    assert_eq!(status(&s, "shop", "b").accepted.reason, "Accepted");
    assert_eq!(retry_policy(&s, "shop", "cart", "shop/cart").0, 2);
}

fn mesh_cfg(
    ns: &str,
    name: &str,
    svc: &str,
    outlier: Option<OutlierSpec>,
    breaker: Option<BreakerSpec>,
) -> Config {
    Config {
        outlier_detection: outlier,
        circuit_breaker: breaker,
        ..cfg(ns, name, 1, &[(TargetKind::Service, svc)])
    }
}

fn outlier_spec(
    failures: Option<u32>,
    fives: Option<u32>,
    base: Option<u32>,
    percent: Option<u32>,
) -> OutlierSpec {
    OutlierSpec {
        consecutive_failures: failures,
        consecutive_5xx: fives,
        base_ejection_seconds: base,
        max_ejection_percent: percent,
    }
}

fn breaker_spec(requests: Option<u32>, connections: Option<u32>) -> BreakerSpec {
    BreakerSpec {
        max_requests: requests,
        max_connections: connections,
    }
}

fn od(failures: u32, fives: u32, base: u32, percent: u32) -> OutlierDetection {
    OutlierDetection {
        consecutive_failures: failures,
        consecutive_5xx: fives,
        base_ejection_seconds: base,
        max_ejection_percent: percent,
    }
}

fn cb(requests: u32, connections: u32) -> CircuitBreaker {
    CircuitBreaker {
        max_requests: requests,
        max_connections: connections,
    }
}

fn mesh_policy(s: &Snapshot, ns: &str, d: &str, key: &str) -> (OutlierDetection, CircuitBreaker) {
    let p = &view_of(s, ns, d).view.services[key].policy;
    (p.outlier_detection.clone(), p.circuit_breaker.clone())
}

#[test]
fn cbr_01_the_defaults_without_any_config() {
    let s = cluster();
    assert_eq!(defaults().outlier_detection, od(5, 0, 30, 50));
    assert_eq!(defaults().circuit_breaker, cb(1024, 1024));
    assert_eq!(effective(None), defaults());
    assert_eq!(
        mesh_policy(&s, "shop", "cart", "shop/cart"),
        (od(5, 0, 30, 50), cb(1024, 1024))
    );
    let mut s = cluster();
    s.configs = vec![mesh_cfg("shop", "a", "cart", None, None)];
    assert_eq!(
        mesh_policy(&s, "shop", "cart", "shop/cart"),
        (od(5, 0, 30, 50), cb(1024, 1024))
    );
}

#[test]
fn cbr_01_each_field_the_winner_sets_is_taken_and_the_others_keep_their_default() {
    let mut s = cluster();
    for (set, want) in [
        (outlier_spec(Some(1), None, None, None), od(1, 0, 30, 50)),
        (
            outlier_spec(Some(100), None, None, None),
            od(100, 0, 30, 50),
        ),
        (outlier_spec(None, Some(0), None, None), od(5, 0, 30, 50)),
        (
            outlier_spec(None, Some(100), None, None),
            od(5, 100, 30, 50),
        ),
        (outlier_spec(None, None, Some(1), None), od(5, 0, 1, 50)),
        (outlier_spec(None, None, Some(300), None), od(5, 0, 300, 50)),
        (outlier_spec(None, None, None, Some(0)), od(5, 0, 30, 0)),
        (outlier_spec(None, None, None, Some(100)), od(5, 0, 30, 100)),
        (outlier_spec(None, None, None, None), od(5, 0, 30, 50)),
        (
            outlier_spec(Some(2), Some(3), Some(4), Some(5)),
            od(2, 3, 4, 5),
        ),
    ] {
        s.configs = vec![mesh_cfg("shop", "a", "cart", Some(set), None)];
        assert_eq!(
            mesh_policy(&s, "shop", "cart", "shop/cart"),
            (want, cb(1024, 1024))
        );
    }
    for (set, want) in [
        (breaker_spec(Some(1), None), cb(1, 1024)),
        (breaker_spec(Some(100_000), None), cb(100_000, 1024)),
        (breaker_spec(None, Some(1)), cb(1024, 1)),
        (breaker_spec(None, Some(100_000)), cb(1024, 100_000)),
        (breaker_spec(None, None), cb(1024, 1024)),
    ] {
        s.configs = vec![mesh_cfg("shop", "a", "cart", None, Some(set))];
        assert_eq!(
            mesh_policy(&s, "shop", "cart", "shop/cart"),
            (od(5, 0, 30, 50), want)
        );
    }
}

#[test]
fn cbr_01_a_consented_service_carries_the_config_of_its_own_namespace() {
    let mut s = cluster();
    s.grants = Some(vec![grant("risk", "shop")]);
    s.configs = vec![
        dep_cfg("shop", "d", 1, "cart", &["risk/scoring"]),
        mesh_cfg(
            "risk",
            "p",
            "scoring",
            Some(outlier_spec(Some(7), None, None, None)),
            Some(breaker_spec(Some(9), None)),
        ),
        mesh_cfg(
            "shop",
            "q",
            "cart",
            Some(outlier_spec(Some(2), None, None, None)),
            None,
        ),
    ];
    assert_eq!(
        mesh_policy(&s, "shop", "cart", "risk/scoring"),
        (od(7, 0, 30, 50), cb(9, 1024))
    );
    assert_eq!(
        mesh_policy(&s, "shop", "cart", "shop/cart"),
        (od(2, 0, 30, 50), cb(1024, 1024))
    );
}

#[test]
fn cbr_01_validate_refuses_each_field_out_of_bounds_naming_it() {
    let svc = |o, b| mesh_cfg("shop", "x", "cart", o, b);
    for ok in [
        outlier_spec(Some(1), Some(0), Some(1), Some(0)),
        outlier_spec(Some(100), Some(100), Some(300), Some(100)),
        outlier_spec(None, None, None, None),
    ] {
        assert!(validate(&svc(Some(ok), None)).is_ok());
    }
    for ok in [
        breaker_spec(Some(1), Some(1)),
        breaker_spec(Some(100_000), Some(100_000)),
        breaker_spec(None, None),
    ] {
        assert!(validate(&svc(None, Some(ok))).is_ok());
    }
    for (bad, field) in [
        (
            outlier_spec(Some(0), None, None, None),
            "consecutiveFailures",
        ),
        (
            outlier_spec(Some(101), None, None, None),
            "consecutiveFailures",
        ),
        (outlier_spec(None, Some(101), None, None), "consecutive5xx"),
        (outlier_spec(None, None, Some(0), None), "baseEjectionTime"),
        (
            outlier_spec(None, None, Some(301), None),
            "baseEjectionTime",
        ),
        (
            outlier_spec(None, None, None, Some(101)),
            "maxEjectionPercent",
        ),
    ] {
        let err = validate(&svc(Some(bad), None)).expect_err("out of bounds");
        assert!(err.contains(field), "{err}");
    }
    for (bad, field) in [
        (breaker_spec(Some(0), None), "maxRequests"),
        (breaker_spec(Some(100_001), None), "maxRequests"),
        (breaker_spec(None, Some(0)), "maxConnections"),
        (breaker_spec(None, Some(100_001)), "maxConnections"),
    ] {
        let err = validate(&svc(None, Some(bad))).expect_err("out of bounds");
        assert!(err.contains(field), "{err}");
    }
}

#[test]
fn cbr_01_both_objects_need_a_service_target() {
    let dep = |o, b| Config {
        outlier_detection: o,
        circuit_breaker: b,
        ..cfg("shop", "x", 1, &[(TargetKind::Deployment, "cart")])
    };
    let err = validate(&dep(Some(outlier_spec(None, None, None, None)), None)).unwrap_err();
    assert!(err.contains("outlierDetection"), "{err}");
    let err = validate(&dep(None, Some(breaker_spec(None, None)))).unwrap_err();
    assert!(err.contains("circuitBreaker"), "{err}");
    let both = Config {
        outlier_detection: Some(outlier_spec(Some(2), None, None, None)),
        ..cfg(
            "shop",
            "x",
            1,
            &[
                (TargetKind::Deployment, "cart"),
                (TargetKind::Service, "cart"),
            ],
        )
    };
    assert!(validate(&both).is_ok());
}
