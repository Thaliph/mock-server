//! MSH-03: consent of the destination's namespace (spec §7.9, review B2 of #469).

use super::input::*;
use super::policy::statuses;
use super::testkit::*;

/// `shop/cart` declares `risk/scoring`; `grants` is what the cluster holds.
fn declared(grants: Option<Vec<ReferenceGrant>>, service_exists: bool) -> Snapshot {
    let mut s = snapshot();
    s.services = vec![http_svc("shop", "cart", "10.96.0.1")];
    if service_exists {
        s.services.push(http_svc("risk", "scoring", "10.50.0.1"));
        s.slices = vec![slice(
            "risk",
            "scoring-1",
            "scoring",
            1,
            9000,
            vec![ep("10.50.1.1", true)],
        )];
    }
    s.deployments = vec![("shop".into(), "cart".into())];
    s.configs = vec![dep_cfg("shop", "cart", 1, "cart", &["risk/scoring"])];
    s.grants = grants;
    s
}

fn outcome(s: &Snapshot) -> (bool, bool, String, String) {
    let present = view_of(s, "shop", "cart")
        .view
        .services
        .contains_key("risk/scoring");
    let st = &statuses(s)[&("shop".to_string(), "cart".to_string())];
    (
        present,
        st.resolved_refs.ok,
        st.resolved_refs.reason.clone(),
        st.resolved_refs.message.clone(),
    )
}

fn from(group: &str, kind: &str, ns: &str) -> GrantFrom {
    GrantFrom {
        group: group.into(),
        kind: kind.into(),
        namespace: ns.into(),
    }
}

fn to(group: &str, kind: &str, name: Option<&str>) -> GrantTo {
    GrantTo {
        group: group.into(),
        kind: kind.into(),
        name: name.map(Into::into),
    }
}

fn grant_in(ns: &str, from: GrantFrom, to: GrantTo) -> Vec<ReferenceGrant> {
    vec![ReferenceGrant {
        namespace: ns.into(),
        from: vec![from],
        to: vec![to],
    }]
}

fn good_from() -> GrantFrom {
    from(MIMIC_GROUP, "MimicConfig", "shop")
}

fn good_to() -> GrantTo {
    to("", "Service", None)
}

#[test]
fn msh_03_each_wrong_grant_leaves_the_destination_out_and_says_ref_not_permitted() {
    let cases: Vec<(&str, Option<Vec<ReferenceGrant>>)> = vec![
        (
            "from namespace",
            Some(grant_in(
                "risk",
                from(MIMIC_GROUP, "MimicConfig", "other"),
                good_to(),
            )),
        ),
        (
            "from kind",
            Some(grant_in(
                "risk",
                from(MIMIC_GROUP, "HTTPRoute", "shop"),
                good_to(),
            )),
        ),
        (
            "from group",
            Some(grant_in(
                "risk",
                from("gateway.networking.k8s.io", "MimicConfig", "shop"),
                good_to(),
            )),
        ),
        (
            "to kind",
            Some(grant_in("risk", good_from(), to("", "Secret", None))),
        ),
        (
            "to group",
            Some(grant_in("risk", good_from(), to("apps", "Service", None))),
        ),
        (
            "to name",
            Some(grant_in(
                "risk",
                good_from(),
                to("", "Service", Some("other")),
            )),
        ),
        (
            "grant in a third namespace",
            Some(grant_in("third", good_from(), good_to())),
        ),
        (
            "grant in the source namespace",
            Some(grant_in("shop", good_from(), good_to())),
        ),
        ("no grant", Some(vec![])),
        ("ReferenceGrant CRD absent", None),
    ];
    for (case, grants) in cases {
        for exists in [true, false] {
            let (present, ok, reason, message) = outcome(&declared(grants.clone(), exists));
            assert!(!present, "{case}: the destination is in the view");
            assert!(!ok, "{case}: ResolvedRefs is True");
            assert_eq!(reason, "RefNotPermitted", "{case}");
            assert!(
                message.contains("risk/scoring"),
                "{case}: the message does not name the destination"
            );
        }
    }
}

#[test]
fn msh_03_a_matching_grant_puts_the_destination_in_the_view() {
    let cases = [
        ("any name", good_to()),
        ("empty name", to("", "Service", Some(""))),
        ("equal name", to("", "Service", Some("scoring"))),
    ];
    for (case, t) in cases {
        let (present, ok, reason, _) =
            outcome(&declared(Some(grant_in("risk", good_from(), t)), true));
        assert!(present && ok && reason == "ResolvedRefs", "{case}");
    }
}

#[test]
fn msh_03_one_matching_entry_among_wrong_ones_is_enough() {
    let g = vec![ReferenceGrant {
        namespace: "risk".into(),
        from: vec![from(MIMIC_GROUP, "MimicConfig", "other"), good_from()],
        to: vec![to("", "Secret", None), to("", "Service", Some("scoring"))],
    }];
    assert!(outcome(&declared(Some(g), true)).0);
}

#[test]
fn msh_03_removing_the_grant_removes_the_destination() {
    let mut s = declared(Some(vec![grant("risk", "shop")]), true);
    assert!(outcome(&s).0);
    s.grants = Some(vec![]);
    let (present, ok, _, _) = outcome(&s);
    assert!(!present && !ok);
}

#[test]
fn msh_03_only_the_denied_destination_is_named_and_a_same_namespace_one_needs_no_grant() {
    let mut s = declared(Some(vec![grant("risk", "shop")]), true);
    s.services.push(http_svc("pay", "ledger", "10.60.0.1"));
    s.services.push(http_svc("shop", "db", "10.96.0.9"));
    s.configs = vec![dep_cfg(
        "shop",
        "cart",
        1,
        "cart",
        &["risk/scoring", "pay/ledger", "shop/db"],
    )];
    let (present, ok, _, message) = outcome(&s);
    assert!(present && !ok);
    assert!(
        message.contains("pay/ledger")
            && !message.contains("risk/scoring")
            && !message.contains("shop/db")
    );
    s.configs = vec![dep_cfg("shop", "cart", 1, "cart", &["shop/db"])];
    s.grants = None;
    assert!(outcome(&s).1);
}
