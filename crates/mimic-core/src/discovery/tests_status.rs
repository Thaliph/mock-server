//! CTL-06: the `MimicWorkload` status is a pure function (spec §7.4).

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;

use super::status::{Meta, discovery, rfc3339_seconds};

const NOW: u64 = 1_793_527_200_000; // 2026-11-01T10:00:00Z

fn meta(inbound: u64, outbound: u64) -> Meta {
    Meta {
        inbound,
        outbound,
        exchanges: 34_567,
        sample_bytes: 1_048_576,
        updated_ms: NOW - 61_999,
    }
}

fn pods(ages_ms: &[u64]) -> BTreeMap<String, u64> {
    ages_ms
        .iter()
        .enumerate()
        .map(|(i, age)| (format!("pod-{i}"), NOW - age))
        .collect()
}

#[test]
fn ctl_06_status_of_a_deployment_with_data() {
    let status = discovery(Some(&meta(12, 0)), &pods(&[0, 1000]), 300_000, NOW);
    assert_eq!(
        status,
        json!({
            "endpoints": {"inbound": 12},
            "exchanges": 34_567,
            "sampleBytes": 1_048_576,
            "pods": 2,
            "lastPushTime": "2026-11-01T09:58:58Z",
        })
    );
}

#[test]
fn ctl_06_directions_without_endpoints_are_omitted() {
    let both = discovery(Some(&meta(3, 4)), &pods(&[]), 1000, NOW);
    assert_eq!(both["endpoints"], json!({"inbound": 3, "outbound": 4}));
    let outbound = discovery(Some(&meta(0, 4)), &pods(&[]), 1000, NOW);
    assert_eq!(outbound["endpoints"], json!({"outbound": 4}));
    let none = discovery(Some(&meta(0, 0)), &pods(&[]), 1000, NOW);
    assert_eq!(none["endpoints"], json!({}));
}

#[test]
fn ctl_06_without_data_the_counts_are_zero_and_last_push_is_absent() {
    let status = discovery(None, &pods(&[0, 5]), 300_000, NOW);
    assert_eq!(
        status,
        json!({"endpoints": {}, "exchanges": 0, "sampleBytes": 0, "pods": 0})
    );
    assert!(status.get("lastPushTime").is_none());
}

#[test]
fn ctl_06_a_pod_counts_exactly_when_its_last_push_is_within_the_window() {
    let window = 30_000;
    let count =
        |ages: &[u64]| discovery(Some(&meta(1, 0)), &pods(ages), window, NOW)["pods"].clone();
    assert_eq!(count(&[29_999, 30_000]), json!(2));
    assert_eq!(count(&[30_001]), json!(0));
    assert_eq!(count(&[0, 30_001, 31_000, 30_000]), json!(2));
}

#[test]
fn ctl_06_a_window_longer_than_the_clock_counts_every_pod() {
    let status = discovery(Some(&meta(1, 0)), &pods(&[10]), u64::MAX, NOW);
    assert_eq!(status["pods"], json!(1));
}

#[test]
fn ctl_06_rfc3339_is_utc_in_seconds() {
    assert_eq!(rfc3339_seconds(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_seconds(999), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_seconds(1_000), "1970-01-01T00:00:01Z");
    assert_eq!(rfc3339_seconds(NOW), "2026-11-01T10:00:00Z");
    assert_eq!(rfc3339_seconds(951_782_400_000), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339_seconds(4_102_444_799_000), "2099-12-31T23:59:59Z");
}

proptest! {
    #[test]
    fn ctl_06_same_input_same_bytes(
        inbound in 0u64..1000,
        outbound in 0u64..1000,
        ages in proptest::collection::vec(0u64..600_000, 0..8),
        window in 1u64..600_000,
    ) {
        let (m, p) = (meta(inbound, outbound), pods(&ages));
        let a = serde_json::to_vec(&discovery(Some(&m), &p, window, NOW)).unwrap();
        let b = serde_json::to_vec(&discovery(Some(&m), &p.clone(), window, NOW)).unwrap();
        prop_assert_eq!(a, b);
    }

    #[test]
    fn ctl_06_pod_count_matches_the_window_rule(
        ages in proptest::collection::vec(0u64..600_000, 0..8),
        window in 1u64..600_000,
    ) {
        let expected = ages.iter().filter(|a| **a <= window).count();
        let status = discovery(Some(&meta(1, 0)), &pods(&ages), window, NOW);
        prop_assert_eq!(status["pods"].as_u64(), Some(expected as u64));
    }
}
