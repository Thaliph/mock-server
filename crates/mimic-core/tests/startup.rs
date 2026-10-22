//! Critère CTL-36 de la spec (§5.5.5) : barrière de démarrage d'un sidecar piloté, au niveau
//! de `mimic-core` (fonction pure).

use mimic_core::startup::{Fallback, Gate, Phase};
use proptest::prelude::*;

const FALLBACKS: [Fallback; 4] = [
    Fallback::Record,
    Fallback::Passthrough,
    Fallback::MockLocal,
    Fallback::Fail,
];

#[test]
fn ctl_36_closed_until_aligned() {
    for f in FALLBACKS {
        let mut g = Gate::new(f);
        assert_eq!(g.phase(), Phase::Waiting);
        assert!(!g.is_open());
        // Un état reçu sans son snapshot ne suffit pas avant l'échéance.
        g.received();
        assert!(!g.is_open(), "{f:?}");
        g.aligned();
        assert_eq!(g.phase(), Phase::Aligned);
        assert!(g.is_open());
        // Une échéance après l'alignement ne change rien.
        assert_eq!(g.timeout(), None);
        assert_eq!(g.phase(), Phase::Aligned);
    }
}

#[test]
fn ctl_36_timeout_without_known_state_applies_the_fallback() {
    for f in [Fallback::Record, Fallback::Passthrough, Fallback::MockLocal] {
        let mut g = Gate::new(f);
        assert_eq!(g.timeout(), Some(f));
        assert_eq!(g.phase(), Phase::Fallback, "{f:?}");
        assert!(g.is_open());
        // Le contrôleur répond ensuite : aligné.
        g.aligned();
        assert_eq!(g.phase(), Phase::Aligned);
    }
    // `fail` : la barrière reste fermée, aucun mode n'est imposé.
    let mut g = Gate::new(Fallback::Fail);
    assert_eq!(g.timeout(), None);
    assert_eq!(g.phase(), Phase::Waiting);
    assert!(!g.is_open());
    // Un état reçu après l'échéance ouvre, même avec `fail` (snapshot encore absent).
    g.received();
    assert_eq!(g.phase(), Phase::LastKnown);
    assert!(g.is_open());
}

#[test]
fn ctl_36_timeout_with_known_state_keeps_it() {
    for f in FALLBACKS {
        // État rechargé du disque, ou reçu sans son snapshot : pas de repli.
        let mut g = Gate::new(f);
        g.received();
        assert_eq!(g.timeout(), None, "{f:?}");
        assert_eq!(g.phase(), Phase::LastKnown);
        assert!(g.is_open());
        g.aligned();
        assert_eq!(g.phase(), Phase::Aligned);
    }
    // Repli appliqué, puis un état reçu sans son snapshot : il remplace le repli.
    let mut g = Gate::new(Fallback::Record);
    assert_eq!(g.timeout(), Some(Fallback::Record));
    g.received();
    assert_eq!(g.phase(), Phase::LastKnown);
}

#[test]
fn ctl_36_waits_in_mock_only_without_known_state_and_a_strict_fallback() {
    for f in FALLBACKS {
        let strict = matches!(f, Fallback::MockLocal | Fallback::Fail);
        let mut g = Gate::new(f);
        assert_eq!(g.waits_in_mock(), strict, "{f:?}");
        g.received();
        assert!(!g.waits_in_mock(), "{f:?}");
    }
    assert!(Fallback::MockLocal.mocks() && Fallback::Fail.mocks());
    assert!(!Fallback::Record.mocks() && !Fallback::Passthrough.mocks());
}

#[test]
fn ctl_36_names() {
    let names: Vec<&str> = Phase::ALL.iter().map(|p| p.as_str()).collect();
    assert_eq!(names, ["waiting", "aligned", "last-known", "fallback"]);
    for p in Phase::ALL {
        assert_eq!(Phase::from_u8(p.to_u8()), p);
    }
    let names: Vec<&str> = FALLBACKS.iter().map(|f| f.as_str()).collect();
    assert_eq!(names, ["record", "passthrough", "mock-local", "fail"]);
    assert_eq!(Fallback::default(), Fallback::Record);
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Received,
    Aligned,
    Timeout,
}

fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        Just(Event::Received),
        Just(Event::Aligned),
        Just(Event::Timeout)
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Dans n'importe quel ordre : une barrière ouverte ne se referme jamais, l'alignement
    /// ouvre toujours, et `fail` sans aucun état reçu n'ouvre jamais.
    #[test]
    fn ctl_36_any_order(f in 0usize..4, events in prop::collection::vec(event(), 0..12)) {
        let f = FALLBACKS[f];
        let mut g = Gate::new(f);
        let mut was_open = false;
        let mut any_state = false;
        let mut fallbacks = 0;
        for e in events {
            match e {
                Event::Received => { g.received(); any_state = true; }
                Event::Aligned => {
                    g.aligned();
                    any_state = true;
                    prop_assert_eq!(g.phase(), Phase::Aligned);
                }
                Event::Timeout => {
                    if let Some(applied) = g.timeout() {
                        prop_assert_eq!(applied, f);
                        prop_assert!(!any_state, "repli malgré un état connu");
                        fallbacks += 1;
                    }
                }
            }
            prop_assert!(!was_open || g.is_open(), "barrière refermée");
            if f == Fallback::Fail && !any_state {
                prop_assert!(!g.is_open());
            }
            was_open = g.is_open();
        }
        prop_assert!(fallbacks <= 1, "repli appliqué deux fois");
    }
}
