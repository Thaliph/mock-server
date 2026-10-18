//! Critères CTL-16 et CTL-18 de la spec (§5.5.3) : sélection des exemples par endpoint, au
//! niveau de `mimic-core` (fonctions pures).

mod common;

use common::{ExchangeExt, exchange};
use mimic_core::exemplar::{ExemplarOffer, Exemplars, endpoint_key, is_exemplar};
use mimic_core::model::{Body, Direction, Exchange};
use mimic_core::snapshot::{SnapshotOptions, build};
use proptest::prelude::*;
use serde_json::json;

fn out(path: &str, at: u64, id: &str) -> Exchange {
    let mut e = exchange("GET", path).at(at, id);
    e.direction = Direction::Outbound;
    e.authority = "api.saas.io:443".into();
    e.resp_json(json!({ "id": id }))
}

fn bodyless(mut e: Exchange) -> Exchange {
    e.resp_body = Body::Skipped { size: 9 };
    e
}

/// Identifiants des exemples gardés, dans l'ordre des endpoints.
fn ids(x: &Exemplars) -> Vec<String> {
    x.iter().map(|e| e.id.clone()).collect()
}

#[test]
fn ctl_16_the_most_recent_replayable_exchange_is_the_example() {
    let mut x = Exemplars::new(10);
    assert_eq!(x.offer(&out("/users/1", 10, "a")), ExemplarOffer::Added);
    // Même forme de chemin, même code : même endpoint, plus récent.
    assert_eq!(x.offer(&out("/users/2", 20, "b")), ExemplarOffer::Replaced);
    // Plus ancien : ne remplace pas.
    assert_eq!(x.offer(&out("/users/3", 5, "c")), ExemplarOffer::Ignored);
    // Plus récent mais sans body rejouable : ne remplace pas.
    assert_eq!(
        x.offer(&bodyless(out("/users/4", 30, "d"))),
        ExemplarOffer::Ignored
    );
    assert_eq!(ids(&x), ["b"]);
    assert_eq!(x.len(), 1);
    // Autre code : autre endpoint.
    assert_eq!(
        x.offer(&out("/users/5", 1, "e").status(404)),
        ExemplarOffer::Added
    );
    assert_eq!(x.len(), 2);
    // Même date : l'identifiant départage.
    assert_eq!(x.offer(&out("/users/6", 20, "a2")), ExemplarOffer::Ignored);
    assert_eq!(x.offer(&out("/users/6", 20, "b2")), ExemplarOffer::Replaced);
}

#[test]
fn ctl_16_what_is_replayable_depends_on_the_direction() {
    // Sortant : la réponse est rejouée ; un body de requête écarté ne gêne pas.
    let mut o = out("/a", 1, "o");
    o.req_body = Body::Skipped { size: 3 };
    assert!(is_exemplar(&o));
    assert!(!is_exemplar(&bodyless(out("/a", 1, "o2"))));
    let mut t = out("/a", 1, "t");
    t.resp_body = Body::Truncated {
        size: 10,
        prefix: vec![b'{'],
    };
    assert!(!is_exemplar(&t));
    // Entrant : la requête est rejouée (§5.7) ; une réponse écartée ne gêne pas.
    let mut i = exchange("POST", "/orders").req_json(json!({"n": 1}));
    i.resp_body = Body::Skipped { size: 9 };
    assert!(is_exemplar(&i));
    let mut j = exchange("POST", "/orders");
    j.req_body = Body::Skipped { size: 9 };
    assert!(!is_exemplar(&j));
    // Poignée de main WebSocket, échange inachevé : jamais rejoués.
    assert!(!is_exemplar(&out("/ws", 1, "w").status(101)));
    assert!(!is_exemplar(&out("/u", 1, "u").status(0)));
    let mut x = Exemplars::new(10);
    assert_eq!(
        x.offer(&out("/ws", 1, "w").status(101)),
        ExemplarOffer::Ignored
    );
    assert!(x.is_empty());
}

#[test]
fn ctl_16_endpoint_is_operation_and_code() {
    let a = out("/users/1", 1, "a");
    let b = out("/users/42", 2, "b");
    assert_eq!(endpoint_key(&a), endpoint_key(&b));
    assert_ne!(endpoint_key(&a), endpoint_key(&a.clone().status(500)));
    let mut inbound = a.clone();
    inbound.direction = Direction::Inbound;
    assert_ne!(endpoint_key(&a), endpoint_key(&inbound));
    let mut post = a.clone();
    post.method = "POST".into();
    assert_ne!(endpoint_key(&a), endpoint_key(&post));
}

#[test]
fn ctl_16_examples_roundtrip_through_jsonl() {
    let mut x = Exemplars::new(10);
    x.offer(&out("/b", 2, "b"));
    x.offer(&out("/a", 1, "a"));
    let bytes = x.to_jsonl();
    let mut y = Exemplars::new(10);
    for e in Exemplars::parse(&bytes) {
        y.offer(&e);
    }
    assert_eq!(y.to_jsonl(), bytes);
    assert_eq!(ids(&y), ids(&x));
    // Une ligne illisible est ignorée.
    let mut damaged = bytes.clone();
    damaged.extend_from_slice(b"{pas du json\n");
    assert_eq!(Exemplars::parse(&damaged).len(), 2);
}

#[test]
fn ctl_16_snapshot_keeps_the_replayable_version_of_an_exchange() {
    // Le même échange, dépouillé de son body en mémoire et gardé entier comme exemple.
    let full = out("/a", 1, "x");
    let stripped = bodyless(full.clone());
    for list in [
        vec![stripped.clone(), full.clone()],
        vec![full.clone(), stripped.clone()],
    ] {
        let s = build(list, &SnapshotOptions::default());
        assert_eq!(s.exchanges, [full.clone()]);
    }
}

#[test]
fn ctl_18_examples_are_bounded_per_workload() {
    let mut x = Exemplars::new(2);
    assert_eq!(x.offer(&out("/a", 1, "a1")), ExemplarOffer::Added);
    assert_eq!(x.offer(&out("/b", 2, "b1")), ExemplarOffer::Added);
    assert_eq!(x.offer(&out("/c", 3, "c1")), ExemplarOffer::Refused);
    assert_eq!(x.refused(), 1);
    // Les exemples gardés restent remplaçables.
    assert_eq!(x.offer(&out("/a", 4, "a2")), ExemplarOffer::Replaced);
    assert_eq!(ids(&x), ["a2", "b1"]);
    // Un échange non rejouable d'un nouvel endpoint n'est pas un refus.
    assert_eq!(
        x.offer(&bodyless(out("/d", 5, "d1"))),
        ExemplarOffer::Ignored
    );
    assert_eq!(x.refused(), 1);
}

#[test]
fn ctl_16_changes_are_counted_for_persistence() {
    let mut x = Exemplars::new(10);
    let v0 = x.version();
    x.offer(&out("/a", 1, "a"));
    let v1 = x.version();
    assert_ne!(v0, v1);
    x.offer(&out("/a", 0, "old"));
    assert_eq!(x.version(), v1, "rien n'a changé");
    x.offer(&out("/a", 2, "new"));
    assert_ne!(x.version(), v1);
}

/// Échange d'endpoint `p` (chemin de forme `/p{p}/{n}`), rejouable ou non, éventuellement
/// de code `101`.
fn sample(p: u8, n: u8, at: u64, replayable: bool, ws: bool) -> Exchange {
    let mut e = out(&format!("/p{p}/{n}"), at, &format!("{p}-{n}-{at}"));
    if !replayable {
        e = bodyless(e);
    }
    if ws {
        e = e.status(101);
    }
    if p % 2 == 1 {
        e = e.status(500);
    }
    e
}

proptest! {
    #[test]
    fn ctl_16_examples_do_not_depend_on_the_order(
        items in proptest::collection::vec(
            (0u8..5, 0u8..4, 0u64..30, any::<bool>(), proptest::bool::weighted(0.1)),
            0..60,
        ),
        seed in any::<u64>(),
    ) {
        let recs: Vec<Exchange> = items
            .iter()
            .map(|(p, n, at, r, ws)| sample(*p, *n, *at, *r, *ws))
            .collect();
        let mut shuffled = recs.clone();
        let n = shuffled.len().max(1);
        shuffled.rotate_left((seed as usize) % n);
        shuffled.reverse();
        if let Some(first) = recs.first() {
            shuffled.push(first.clone());
        }
        let (mut a, mut b) = (Exemplars::new(100), Exemplars::new(100));
        for e in &recs {
            a.offer(e);
        }
        for e in &shuffled {
            b.offer(e);
        }
        prop_assert_eq!(a.to_jsonl(), b.to_jsonl());

        // Un exemple par endpoint ayant un échange rejouable : le plus récent.
        for e in a.iter() {
            prop_assert!(is_exemplar(e));
            let best = recs
                .iter()
                .filter(|r| is_exemplar(r) && endpoint_key(r) == endpoint_key(e))
                .map(|r| (r.started_at_ms, r.id.clone()))
                .max();
            prop_assert_eq!(best, Some((e.started_at_ms, e.id.clone())));
        }
        let endpoints: std::collections::BTreeSet<String> = recs
            .iter()
            .filter(|r| is_exemplar(r))
            .map(endpoint_key)
            .collect();
        prop_assert_eq!(a.len(), endpoints.len());
    }
}
