//! Critères CTL-11 et CTL-12 de la spec (§5.5.2), au niveau `mimic-core` : plafond de
//! bodies par opération et compaction des segments.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::compact::{BodySamples, Compactor, hour_of, hour_start};
use mimic_core::model::{Body, Exchange};
use mimic_core::sample::priority;
use proptest::prelude::*;
use serde_json::json;

fn with_body(path: &str, status: u16, id: &str) -> Exchange {
    exchange("GET", path)
        .status(status)
        .at(1, id)
        .resp_json(json!({ "id": id }))
}

fn jsonl(list: &[Exchange]) -> Vec<u8> {
    list.iter()
        .flat_map(|e| {
            let mut l = serde_json::to_vec(e).unwrap();
            l.push(b'\n');
            l
        })
        .collect()
}

fn parse(bytes: &[u8]) -> Vec<Exchange> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

#[test]
fn ctl_12_bodies_are_capped_per_operation_and_code() {
    let mut samples = BodySamples::new(2);
    let mut kept = vec![];
    for i in 0..3 {
        let mut e = with_body("/a", 200, &format!("a-{i}"));
        kept.push(samples.admit(&mut e));
        if i == 2 {
            assert_eq!(e.resp_body, Body::Skipped { size: 12 });
        }
    }
    assert_eq!(kept, [true, true, false]);
    // Autre code, autre opération : leurs propres places.
    assert!(samples.admit(&mut with_body("/a", 500, "a-500")));
    assert!(samples.admit(&mut with_body("/b", 200, "b-0")));
    // Un échange sans body ne prend pas de place.
    let mut empty = exchange("GET", "/a").at(1, "vide");
    assert!(samples.admit(&mut empty));
}

#[test]
fn ctl_12_eviction_frees_a_place() {
    let mut samples = BodySamples::new(1);
    let mut first = with_body("/a", 200, "1");
    assert!(samples.admit(&mut first));
    assert!(!samples.admit(&mut with_body("/a", 200, "2")));
    samples.release(&first);
    let mut third = with_body("/a", 200, "3");
    assert!(samples.admit(&mut third));
    assert!(matches!(third.resp_body, Body::Captured { .. }));
}

#[test]
fn ctl_11_compaction_dedupes_and_caps_bodies() {
    let a = jsonl(&[
        with_body("/a", 200, "pod-a-1"),
        with_body("/a", 200, "pod-a-2"),
        with_body("/a", 200, "pod-a-3"),
    ]);
    let mut b = jsonl(&[
        with_body("/a", 200, "pod-a-2"),
        with_body("/a", 404, "pod-b-1"),
    ]);
    b.extend_from_slice(b"ligne illisible\n");
    let mut c = Compactor::new(2);
    assert_eq!(c.push(&a), 3);
    assert_eq!(c.push(&b), 1, "doublon et ligne illisible ignorés");
    assert_eq!(c.len(), 4);
    let out = parse(&c.finish());
    let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["pod-a-1", "pod-a-2", "pod-a-3", "pod-b-1"]);
    // Pour /a en 200, les deux exemples de plus petite priorité gardent leur body (§5.2).
    let mut by_priority = ["pod-a-1", "pod-a-2", "pod-a-3"];
    by_priority.sort_by_key(|id| priority(id));
    let bodies: Vec<&str> = out
        .iter()
        .filter(|e| matches!(e.resp_body, Body::Captured { .. }))
        .map(|e| e.id.as_str())
        .collect();
    let mut expected = vec![by_priority[0], by_priority[1], "pod-b-1"];
    expected.sort();
    let mut bodies = bodies;
    bodies.sort();
    assert_eq!(bodies, expected);
}

#[test]
fn ctl_11_compaction_keeps_the_same_sample_in_any_order() {
    let list: Vec<Exchange> = (0..12)
        .map(|i| with_body(&format!("/users/{i}"), 200, &format!("pod-a-{i}")))
        .collect();
    let compact = |list: &[Exchange]| {
        let mut c = Compactor::new(3);
        for chunk in list.chunks(5) {
            c.push(&jsonl(chunk));
        }
        let mut kept: Vec<String> = parse(&c.finish())
            .into_iter()
            .filter(|e| matches!(e.resp_body, Body::Captured { .. }))
            .map(|e| e.id)
            .collect();
        kept.sort();
        kept
    };
    let mut reversed = list.clone();
    reversed.reverse();
    let kept = compact(&list);
    assert_eq!(kept.len(), 3, "chemins de même forme : une seule opération");
    assert_eq!(kept, compact(&reversed));
    let mut smallest: Vec<String> = list.iter().map(|e| e.id.clone()).collect();
    smallest.sort_by_key(|id| priority(id));
    smallest.truncate(3);
    smallest.sort();
    assert_eq!(kept, smallest);
}

#[test]
fn ctl_12_paths_of_the_same_shape_share_their_places() {
    let mut samples = BodySamples::new(1);
    assert!(samples.admit(&mut with_body("/users/1", 200, "1")));
    assert!(!samples.admit(&mut with_body("/users/2", 200, "2")));
    assert!(samples.admit(&mut with_body("/users/me", 200, "3")));
}

#[test]
fn ctl_11_hours_are_named_in_utc() {
    assert_eq!(hour_of(0), ("1970010100".to_string(), 0, 3_600_000));
    // 2025-10-09T08:53:20Z.
    let (name, start, end) = hour_of(1_760_000_000_000);
    assert_eq!(name, "2025100908");
    assert_eq!(start, 1_759_996_800_000);
    assert_eq!(end, start + 3_600_000);
    assert_eq!(hour_of(end).0, "2025100909");
    // Passage d'année, année bissextile.
    assert_eq!(hour_of(951_782_400_000).0, "2000022900");
    assert_eq!(hour_of(1_704_067_199_999).0, "2023123123");
    // Le nom se relit.
    assert_eq!(hour_start("2025100908"), Some(start));
    assert_eq!(hour_start("1970010100"), Some(0));
    for bad in [
        "2025023100",
        "2025100924",
        "202510090",
        "abcdefghij",
        "2025130100",
    ] {
        assert_eq!(hour_start(bad), None, "{bad}");
    }
}

proptest! {
    #[test]
    fn ctl_11_hour_names_roundtrip(ms in 0i64..4_102_444_800_000) {
        let (name, start, end) = hour_of(ms);
        prop_assert!(start <= ms && ms < end);
        prop_assert_eq!(hour_start(&name), Some(start));
    }
}

proptest! {
    /// Mêmes segments, même résultat : la compaction est déterministe.
    #[test]
    fn ctl_11_compaction_is_deterministic(
        items in proptest::collection::vec((0u8..3, 0u8..20), 0..40),
        cap in 1usize..4,
    ) {
        let list: Vec<Exchange> = items
            .iter()
            .map(|(p, id)| with_body(&format!("/p{p}"), 200, &format!("{id}")))
            .collect();
        let run = || {
            let mut c = Compactor::new(cap);
            for chunk in list.chunks(7) {
                c.push(&jsonl(chunk));
            }
            c.finish()
        };
        let out = parse(&run());
        prop_assert_eq!(run(), jsonl(&out));
        let mut ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), total, "aucun doublon");
        for p in 0..3 {
            let bodies = out
                .iter()
                .filter(|e| e.path == format!("/p{p}") && matches!(e.resp_body, Body::Captured { .. }))
                .count();
            prop_assert!(bodies <= cap);
        }
    }
}
