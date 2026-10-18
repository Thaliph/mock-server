//! Critère ADM-05 (spec §9.3) : compter une requête n'alloue rien.
//!
//! Seul test de ce binaire : l'allocateur instrumenté compte les allocations de tout le
//! processus, aucun autre test ne doit tourner pendant la mesure.

use std::alloc::System;
use std::time::Duration;

use mimic_core::model::Direction;
use mimic_proxy::config::Mode;
use mimic_proxy::metrics::Metrics;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Ce que le proxy compte pour une requête, sortante puis entrante.
fn count_one(m: &Metrics) {
    for (direction, mode) in [
        (Direction::Outbound, Mode::Mock),
        (Direction::Inbound, Mode::Record),
    ] {
        let a = m.authority("api.saas.io:443");
        m.count_request(direction, mode, &a, "GET", 200);
        m.count_request(direction, mode, &a, "POST", 503);
        m.observe_duration(direction, mode, &a, Duration::from_micros(150));
        m.count_mock(&a, "exact");
        m.observe_mock_latency(&a, Duration::from_micros(150));
        m.count_upstream_error(direction, &a, "connect");
        m.count_dropped("sample_limit");
    }
}

#[test]
fn adm_05_counting_a_request_does_not_allocate() {
    let m = Metrics::new();
    // Première fois : séries et authority créées.
    count_one(&m);
    let region = Region::new(GLOBAL);
    for _ in 0..1_000 {
        count_one(&m);
    }
    let change = region.change();
    assert_eq!(change.allocations, 0, "{change:?}");
    assert_eq!(change.reallocations, 0, "{change:?}");
    let text = m.encode();
    assert!(
        text.contains(
            r#"mimic_requests_total{direction="outbound",mode="mock",authority="api.saas.io:443",method="GET",status="200"} 1001"#
        ),
        "{text}"
    );
    assert!(
        text.contains(
            r#"mimic_mock_responses_total{authority="api.saas.io:443",match="exact"} 2002"#
        ),
        "{text}"
    );
}
