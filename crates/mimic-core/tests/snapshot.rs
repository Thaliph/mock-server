//! Critères SNP-01 et SNP-02 de la spec (§5.6) : construction des snapshots de mock.

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::model::{Direction, Exchange};
use mimic_core::snapshot::{Snapshot, SnapshotOptions, build};
use proptest::prelude::*;
use serde_json::json;

fn out(path: &str, at: u64, id: &str) -> Exchange {
    let mut e = exchange("GET", path).at(at, id);
    e.direction = Direction::Outbound;
    e.authority = "api.saas.io:443".into();
    e.resp_json(json!({ "id": id }))
}

#[test]
fn snp_01_same_exchanges_same_snapshot_whatever_the_order() {
    let a = vec![
        out("/a", 1, "x-1"),
        out("/b", 2, "y-1"),
        out("/a", 3, "x-2"),
    ];
    let mut b = a.clone();
    b.reverse();
    b.push(a[0].clone());
    let opts = SnapshotOptions::default();
    let (sa, sb) = (build(a, &opts), build(b, &opts));
    assert_eq!(sa.id, sb.id);
    assert_eq!(sa.exchanges, sb.exchanges);
    assert_eq!(sa.exchanges.len(), 3);
    assert_eq!(sa.id.len(), 16);
}

#[test]
fn snapshot_keeps_only_finished_outbound_exchanges() {
    let mut inbound = out("/in", 1, "i");
    inbound.direction = Direction::Inbound;
    let mut unfinished = out("/u", 2, "u");
    unfinished.status = 0;
    let s = build(
        vec![inbound, unfinished, out("/ok", 3, "o")],
        &SnapshotOptions::default(),
    );
    let ids: Vec<_> = s.exchanges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["o"]);
}

#[test]
fn snp_02_keeps_the_most_recent_per_key() {
    let recs: Vec<_> = (0..10).map(|i| out("/a", i, &format!("{i:02}"))).collect();
    let s = build(recs, &SnapshotOptions { max_per_key: 3 });
    let ids: Vec<_> = s.exchanges.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["07", "08", "09"]);
}

#[test]
fn different_content_gives_different_id() {
    let opts = SnapshotOptions::default();
    let a = build(vec![out("/a", 1, "1")], &opts);
    let b = build(vec![out("/a", 1, "1").status(500)], &opts);
    assert_ne!(a.id, b.id);
}

#[test]
fn jsonl_roundtrip_checks_the_id() {
    let s = build(
        vec![out("/a", 1, "1"), out("/b", 2, "2")],
        &SnapshotOptions::default(),
    );
    let bytes = s.to_jsonl();
    assert_eq!(Snapshot::from_jsonl(&bytes, &s.id).unwrap(), s);
    let mut tampered = bytes.clone();
    let pos = tampered.iter().position(|b| *b == b'/').unwrap();
    tampered[pos + 1] = b'z';
    assert!(Snapshot::from_jsonl(&tampered, &s.id).is_err());
}

proptest! {
    #[test]
    fn snp_01_order_and_duplicates_do_not_matter(
        items in proptest::collection::vec((0u8..4, 0u64..20, 0u8..30), 0..40),
        seed in any::<u64>(),
    ) {
        let recs: Vec<Exchange> = items
            .iter()
            .map(|(p, at, id)| out(&format!("/p{p}"), *at, &format!("{id}-{p}-{at}")))
            .collect();
        let mut shuffled = recs.clone();
        // Permutation déterministe dérivée de la graine, avec doublons.
        let n = shuffled.len().max(1);
        shuffled.rotate_left((seed as usize) % n);
        if let Some(first) = recs.first() {
            shuffled.push(first.clone());
        }
        let opts = SnapshotOptions { max_per_key: 5 };
        // Le contrôleur construit sur des références (§5.5.2) : même snapshot.
        prop_assert_eq!(build(&shuffled, &opts), build(recs.clone(), &opts));
        prop_assert_eq!(build(recs, &opts), build(shuffled, &opts));
    }
}
