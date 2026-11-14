//! RTE-43, RTE-44, RTE-54: the weighted draw and the header modifiers (spec §7.9 "Routing").

use proptest::prelude::*;

use super::route::{BackendView, FiltersView, ModifierView, NameValue, Pick, draw};
use super::tests_from_json::routed;
use super::view::{Limits, View};

fn backend(weight: u32, invalid: bool) -> BackendView {
    BackendView {
        invalid: invalid.then_some(true),
        port: (!invalid).then_some(80),
        service: (!invalid).then(|| "shop/cart-v1".into()),
        weight,
    }
}

fn backends(weights: &[u32]) -> Vec<BackendView> {
    weights.iter().map(|w| backend(*w, false)).collect()
}

// ---- RTE-43: the draw

#[test]
fn rte_43_no_backend_when_there_is_no_weight() {
    assert_eq!(draw(&[], 0), Pick::NoBackend);
    assert_eq!(draw(&[], u64::MAX), Pick::NoBackend);
    assert_eq!(draw(&backends(&[0, 0, 0]), 12345), Pick::NoBackend);
}

#[test]
fn rte_43_the_ends_of_the_range() {
    let b = backends(&[0, 3, 0, 7, 0]);
    assert_eq!(draw(&b, 0), Pick::Backend(1));
    assert_eq!(draw(&b, u64::MAX), Pick::Backend(3));
    let b = backends(&[5]);
    assert_eq!(draw(&b, 0), Pick::Backend(0));
    assert_eq!(draw(&b, u64::MAX), Pick::Backend(0));
}

#[test]
fn rte_43_an_invalid_backend_drawn_is_invalid() {
    let b = vec![backend(1, true), backend(1, false)];
    assert_eq!(draw(&b, 0), Pick::Invalid);
    assert_eq!(draw(&b, u64::MAX), Pick::Backend(1));
    // weight 0 and invalid is never drawn
    let b = vec![backend(0, true), backend(1, false)];
    assert_eq!(draw(&b, 0), Pick::Backend(1));
}

#[test]
fn rte_43_the_largest_weights_do_not_overflow() {
    let b = backends(&[1_000_000; 16]);
    for r in [0, 1, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
        assert!(matches!(draw(&b, r), Pick::Backend(i) if i < 16));
    }
    assert_eq!(draw(&b, u64::MAX), Pick::Backend(15));
}

proptest! {
    #[test]
    fn rte_43_never_a_weight_zero_backend_and_the_invalid_follow_the_weights(
        spec in proptest::collection::vec((0u32..=1_000_000, any::<bool>()), 0..=16),
        r in any::<u64>(),
    ) {
        let valid: Vec<BackendView> = spec.iter().map(|(w, _)| backend(*w, false)).collect();
        let mixed: Vec<BackendView> = spec.iter().map(|(w, bad)| backend(*w, *bad)).collect();
        let total: u64 = spec.iter().map(|(w, _)| u64::from(*w)).sum();
        match (draw(&valid, r), draw(&mixed, r)) {
            (Pick::NoBackend, mixed_pick) => {
                prop_assert_eq!(total, 0);
                prop_assert_eq!(mixed_pick, Pick::NoBackend);
            }
            (Pick::Backend(i), mixed_pick) => {
                prop_assert!(total > 0 && spec[i].0 > 0, "weight 0 drawn");
                if spec[i].1 {
                    prop_assert_eq!(mixed_pick, Pick::Invalid);
                } else {
                    prop_assert_eq!(mixed_pick, Pick::Backend(i));
                }
            }
            (Pick::Invalid, _) => prop_assert!(false, "valid backends drawn invalid"),
        }
    }

    #[test]
    fn rte_43_each_backend_is_drawn_exactly_its_weight_over_the_k_th_values(
        weights in proptest::collection::vec(0u32..=60, 1..=16),
    ) {
        let total: u128 = weights.iter().map(|w| u128::from(*w)).sum();
        prop_assume!(total > 0);
        let b = backends(&weights);
        let mut count = vec![0u32; weights.len()];
        for k in 0..total {
            // r = ceil(k * 2^64 / W)
            let r = ((k << 64) + total - 1) / total;
            let Pick::Backend(i) = draw(&b, r as u64) else {
                return Err(TestCaseError::fail("no backend with a weight"));
            };
            count[i] += 1;
        }
        prop_assert_eq!(count, weights);
    }

    #[test]
    fn rte_43_zero_draws_the_first_non_zero_and_max_the_last(
        weights in proptest::collection::vec(0u32..=1_000_000, 1..=16),
    ) {
        let first = weights.iter().position(|w| *w > 0);
        let last = weights.iter().rposition(|w| *w > 0);
        let b = backends(&weights);
        match (first, last) {
            (Some(f), Some(l)) => {
                prop_assert_eq!(draw(&b, 0), Pick::Backend(f));
                prop_assert_eq!(draw(&b, u64::MAX), Pick::Backend(l));
            }
            _ => prop_assert_eq!(draw(&b, 0), Pick::NoBackend),
        }
    }
}

// ---- RTE-44: modifiers on a field list

type Fields = Vec<(String, String)>;

fn fields(items: &[(&str, &str)]) -> Fields {
    items
        .iter()
        .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
        .collect()
}

fn nv(items: &[(&str, &str)]) -> Vec<NameValue> {
    items
        .iter()
        .map(|(n, v)| NameValue {
            name: (*n).into(),
            value: (*v).into(),
        })
        .collect()
}

fn modifier(set: &[(&str, &str)], add: &[(&str, &str)], remove: &[&str]) -> ModifierView {
    ModifierView {
        add: nv(add),
        remove: remove.iter().map(|n| (*n).to_string()).collect(),
        set: nv(set),
    }
}

fn applied(m: &ModifierView, start: &[(&str, &str)]) -> Fields {
    let mut f = fields(start);
    m.apply(&mut f);
    f
}

#[test]
fn rte_44_set_replaces_every_line_of_the_name_or_adds_one() {
    let m = modifier(&[("x-a", "new")], &[], &[]);
    assert_eq!(
        applied(&m, &[("x-a", "1"), ("y", "2"), ("X-A", "3")]),
        fields(&[("y", "2"), ("x-a", "new")])
    );
    assert_eq!(
        applied(&m, &[("y", "2")]),
        fields(&[("y", "2"), ("x-a", "new")])
    );
    assert_eq!(applied(&m, &[]), fields(&[("x-a", "new")]));
}

#[test]
fn rte_44_add_keeps_the_existing_lines_and_appends_one() {
    let m = modifier(&[], &[("x-a", "new")], &[]);
    assert_eq!(
        applied(&m, &[("x-a", "1"), ("y", "2")]),
        fields(&[("x-a", "1"), ("y", "2"), ("x-a", "new")])
    );
    assert_eq!(applied(&m, &[]), fields(&[("x-a", "new")]));
}

#[test]
fn rte_44_remove_drops_every_line_of_the_name_and_an_absent_one_is_nothing() {
    let m = modifier(&[], &[], &["x-a", "absent"]);
    assert_eq!(
        applied(&m, &[("x-a", "1"), ("y", "2"), ("X-A", "3")]),
        fields(&[("y", "2")])
    );
    assert_eq!(applied(&m, &[("y", "2")]), fields(&[("y", "2")]));
}

#[test]
fn rte_44_the_order_is_set_then_add_then_remove() {
    let both = modifier(&[("x", "1")], &[], &["x"]);
    assert_eq!(applied(&both, &[("x", "0")]), fields(&[]));
    let two = modifier(&[("x", "1")], &[("x", "2")], &[]);
    assert_eq!(
        applied(&two, &[("x", "0")]),
        fields(&[("x", "1"), ("x", "2")])
    );
    let all = modifier(&[("x", "1")], &[("x", "2")], &["x"]);
    assert_eq!(applied(&all, &[("x", "0")]), fields(&[]));
    // each list in its order
    let m = modifier(&[("a", "1"), ("a", "2")], &[("b", "1"), ("b", "2")], &[]);
    assert_eq!(
        applied(&m, &[]),
        fields(&[("a", "2"), ("b", "1"), ("b", "2")])
    );
}

#[test]
fn rte_44_names_in_any_case_added_names_lower_case_the_rest_untouched() {
    let m = modifier(&[("x-a", "v")], &[("x-b", "w")], &["x-c"]);
    let out = applied(
        &m,
        &[("Host", "h"), ("X-A", "1"), ("X-C", "2"), ("Accept", "*/*")],
    );
    assert_eq!(
        out,
        fields(&[("Host", "h"), ("Accept", "*/*"), ("x-a", "v"), ("x-b", "w")])
    );
    // the modifier is given upper case (a view never holds it): the added name is lower case
    let m = modifier(&[("X-A", "v")], &[("X-B", "w")], &[]);
    assert_eq!(
        applied(&m, &[("x-a", "1")]),
        fields(&[("x-a", "v"), ("x-b", "w")])
    );
}

#[test]
fn rte_44_a_modifier_at_the_crd_maximum_adds_at_most_32_lines_of_4356_bytes() {
    let name = |i: usize| format!("{i:0>256}");
    let value = "v".repeat(4096);
    let set: Vec<(String, String)> = (0..16).map(|i| (name(i), value.clone())).collect();
    let add: Vec<(String, String)> = (16..32).map(|i| (name(i), value.clone())).collect();
    let m = ModifierView {
        set: set
            .iter()
            .map(|(n, v)| NameValue {
                name: n.clone(),
                value: v.clone(),
            })
            .collect(),
        add: add
            .iter()
            .map(|(n, v)| NameValue {
                name: n.clone(),
                value: v.clone(),
            })
            .collect(),
        remove: vec![],
    };
    let mut f: Fields = vec![];
    m.apply(&mut f);
    assert_eq!(f.len(), 32);
    assert_eq!(
        f.iter().map(|(n, v)| n.len() + v.len()).sum::<usize>(),
        32 * 4352
    );
    assert!(f.iter().all(|(n, v)| n.len() + v.len() <= 256 + 4096));
}

// ---- RTE-54: framing invariants

/// Names no route may change (RTE-03, `host`) besides the `x-mimic-` prefix.
const PROTECTED: [&str; 10] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
    "x-mimic-error",
];

fn protected(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PROTECTED.contains(&lower.as_str()) || lower.starts_with("x-mimic-")
}

fn pool_name() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-z][a-z0-9-]{0,8}",
        3 => proptest::sample::select(vec![
            "x-env", "x-canary", "accept", "x-trace", "hostname", "x-content-length", "x-mimic",
        ]).prop_map(String::from),
        1 => proptest::sample::select(PROTECTED.to_vec()).prop_map(String::from),
        1 => "x-mimic-[a-z]{1,6}",
        1 => proptest::sample::select(vec!["Host", "Content-Length", "TE", "Upgrade", "X-Mimic-Id"])
            .prop_map(String::from),
    ]
}

fn pairs(max: usize) -> impl Strategy<Value = Vec<NameValue>> {
    proptest::collection::vec(
        (pool_name(), "[ -~]{0,12}").prop_map(|(name, value)| NameValue {
            name: name.to_ascii_lowercase(),
            value,
        }),
        0..=max,
    )
}

fn generated_modifier() -> impl Strategy<Value = ModifierView> {
    (
        pairs(4),
        pairs(4),
        proptest::collection::vec(pool_name().prop_map(|n| n.to_ascii_lowercase()), 0..=4),
    )
        .prop_map(|(set, add, remove)| ModifierView { add, remove, set })
}

fn head() -> impl Strategy<Value = (String, String, Fields)> {
    (
        proptest::sample::select(vec!["GET", "POST", "PUT", "DELETE", "HEAD"])
            .prop_map(String::from),
        "/[a-z0-9/]{0,12}",
        proptest::collection::vec((pool_name(), "[ -~]{0,12}"), 0..=32),
    )
}

/// The view `routed()` builds with `request` and `response` as the modifiers of its rule, as the
/// sidecar reads it (`from_json`); `None` when the view refuses it.
fn accepted(request: &ModifierView, response: &ModifierView) -> Option<FiltersView> {
    let mut view = routed().view;
    let service = view.services.get_mut("shop/cart")?;
    let filters = service
        .routes
        .get_mut(0)?
        .rules
        .get_mut(0)?
        .filters
        .as_mut()?;
    filters.request_headers = Some(request.clone());
    filters.response_headers = Some(response.clone());
    let back = View::from_json(&view.to_json(), &Limits::default()).ok()?;
    back.services
        .get("shop/cart")?
        .routes
        .first()?
        .rules
        .first()?
        .filters
        .clone()
}

fn lines_of_protected(f: &Fields) -> Vec<(String, String)> {
    f.iter().filter(|(n, _)| protected(n)).cloned().collect()
}

proptest! {
    #[test]
    fn rte_54_a_modifier_a_view_accepts_never_touches_framing_host_or_mimic_fields(
        request in generated_modifier(),
        response in generated_modifier(),
        (method, target, request_head) in head(),
        response_head in proptest::collection::vec((pool_name(), "[ -~]{0,12}"), 0..=32),
    ) {
        let Some(filters) = accepted(&request, &response) else {
            // The view refuses it (a protected name): nothing reaches a sidecar.
            let names = |m: &ModifierView| -> Vec<String> {
                m.set.iter().chain(&m.add).map(|p| p.name.clone()).chain(m.remove.clone()).collect()
            };
            prop_assert!(names(&request).iter().chain(&names(&response)).any(|n| protected(n)));
            return Ok(());
        };
        let (req_mod, resp_mod) = (
            filters.request_headers.expect("kept"),
            filters.response_headers.expect("kept"),
        );
        for (modifier, original) in [(&req_mod, &request_head), (&resp_mod, &response_head)] {
            let before = original.clone();
            let mut after = before.clone();
            modifier.apply(&mut after);
            prop_assert_eq!(lines_of_protected(&after), lines_of_protected(&before));
            prop_assert!(after.len() <= before.len() + 32);
        }
        // The method and the target are not part of the field list: unchanged by construction.
        prop_assert!(!method.is_empty() && target.starts_with('/'));
    }
}

#[test]
fn rte_54_the_generator_reaches_accepted_views() {
    let ok = modifier(&[("x-env", "prod")], &[("x-from", "mesh")], &["x-debug"]);
    assert!(accepted(&ok, &ok).is_some());
    let bad = modifier(&[("host", "evil")], &[], &[]);
    assert!(accepted(&bad, &ok).is_none());
    assert!(accepted(&ok, &bad).is_none());
}
