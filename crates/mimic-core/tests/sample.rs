//! Critères CAP-01, CAP-02 et CAP-04 de la spec (§5.2), niveau cœur : clé d'échantillonnage
//! par forme de chemin et réservoir déterministe des bodies.

use std::collections::BTreeSet;

use mimic_core::model::Direction;
use mimic_core::sample::{Offer, Reservoir, path_shape, priority, sample_key};
use proptest::prelude::*;

/// Rejoue un flux d'identifiants dans un réservoir de capacité `n` ; renvoie les gardés.
fn sample(ids: &[String], n: usize) -> BTreeSet<String> {
    let mut r = Reservoir::default();
    for id in ids {
        r.offer(n, priority(id), id);
    }
    r.ids().map(String::from).collect()
}

fn ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("api-0-{i:05}")).collect()
}

#[test]
fn cap_01_identifiers_in_paths_share_one_operation() {
    assert_eq!(path_shape("/users/1"), path_shape("/users/42"));
    assert_eq!(
        path_shape("/v1/orders/3f2b1c4e-8a6d-4e1f-9b2a-7c5d3e1f0a9b/lines"),
        path_shape("/v1/orders/0b8c2d1e-4f3a-4b5c-8d7e-6f5a4b3c2d1e/lines")
    );
    assert_ne!(path_shape("/users/1"), path_shape("/users/me"));
    assert_ne!(
        path_shape("/users/1"),
        path_shape("/users/3f2b1c4e-8a6d-4e1f-9b2a-7c5d3e1f0a9b"),
        "un entier et un UUID ne sont pas de même nature"
    );
    assert_ne!(path_shape("/users/1"), path_shape("/users/1/orders"));
    assert_eq!(path_shape("/a//b/"), path_shape("/a/b"));

    let key = |d, m, p, s| sample_key(d, "api.saas.io:443", m, p, s);
    let base = key(Direction::Outbound, "GET", "/users/1", 200);
    assert_eq!(base, key(Direction::Outbound, "get", "/users/77", 200));
    assert_ne!(base, key(Direction::Outbound, "GET", "/users/1", 404));
    assert_ne!(base, key(Direction::Outbound, "POST", "/users/1", 200));
    assert_ne!(base, key(Direction::Inbound, "GET", "/users/1", 200));
    assert_ne!(
        base,
        sample_key(Direction::Outbound, "other:443", "GET", "/users/1", 200)
    );
}

#[test]
fn cap_01_reservoir_keeps_at_most_n() {
    let all = ids(100);
    let kept = sample(&all, 20);
    assert_eq!(kept.len(), 20);
    assert!(kept.iter().all(|k| all.contains(k)));
    assert!(sample(&all, 0).is_empty(), "N = 0 : métadonnées seules");
}

#[test]
fn cap_01_offer_reports_what_happens() {
    let mut r = Reservoir::default();
    assert_eq!(r.offer(1, 10, "a"), Offer::Kept);
    assert!(!r.admits(1, 20));
    assert_eq!(r.offer(1, 20, "b"), Offer::Rejected);
    assert!(r.admits(1, 5));
    assert_eq!(r.offer(1, 5, "c"), Offer::Replaced("a".into()));
    assert_eq!(r.ids().collect::<Vec<_>>(), ["c"]);
    assert_eq!(r.offer(0, 1, "d"), Offer::Rejected);
}

#[test]
fn cap_02_late_arrivals_can_be_kept() {
    let all = ids(100);
    let kept = sample(&all, 20);
    let late = kept.iter().filter(|k| !all[..20].contains(k)).count();
    assert!(
        late > 0,
        "réservoir figé sur les premiers échanges : {kept:?}"
    );
}

#[test]
fn cap_02_priority_is_stable() {
    // Hachage figé : un changement de version de Rust ne doit pas changer l'échantillon.
    assert_eq!(priority("api-0-00000"), priority("api-0-00000"));
    assert_ne!(priority("api-0-00000"), priority("api-0-00001"));
    assert_eq!(priority(""), 0xF52A_15E9_A9B5_E89B);
}

#[test]
fn cap_04_removing_frees_a_slot_and_empties_the_reservoir() {
    let mut r = Reservoir::default();
    r.offer(2, priority("a"), "a");
    r.offer(2, priority("b"), "b");
    assert!(r.remove(priority("a"), "a"));
    assert!(!r.remove(priority("a"), "a"));
    assert_eq!(r.len(), 1);
    assert!(
        r.admits(2, u64::MAX),
        "place libérée : tout échange est admis"
    );
    assert!(r.remove(priority("b"), "b"));
    assert!(r.is_empty());
}

proptest! {
    #[test]
    fn cap_02_same_stream_in_any_order_same_sample(
        seed in any::<u64>(),
        n in 0usize..30,
    ) {
        let all = ids(80);
        let mut shuffled = all.clone();
        // Permutation déterministe tirée de la graine (Fisher-Yates).
        let mut s = seed;
        for i in (1..shuffled.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            shuffled.swap(i, (s >> 33) as usize % (i + 1));
        }
        prop_assert_eq!(sample(&all, n), sample(&shuffled, n));
    }
}
