//! GW-01 and GW-07 properties: generated names are valid and stable; the acceptance of any
//! converted Gateway keeps its bounds.

use proptest::prelude::*;

use super::accept::gateway;
use super::objects::name;
use super::testkit::any_gateway;
use super::{ADMIN_PORT, MAX_LISTENERS};

/// A DNS-1035 label of at most 63 characters, written here and not taken from the
/// implementation: `[a-z]([-a-z0-9]*[a-z0-9])?`.
fn dns_1035(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && (b[b.len() - 1].is_ascii_lowercase() || b[b.len() - 1].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn label() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z0-9]([a-z0-9-]{0,18}[a-z0-9])?").expect("label regex")
}

/// DNS-1123 subdomains of at most 63 characters: labels of 1 to 20 characters (digit starts
/// included), and dots around character 46, where the hashed form cuts.
fn subdomain() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::collection::vec(label(), 1..6).prop_map(|labels| labels.join(".")),
        (
            prop::string::string_regex("[a-z0-9][a-z0-9-]{43,45}[a-z0-9]").expect("head regex"),
            label()
        )
            .prop_map(|(head, tail)| format!("{head}.{tail}")),
    ]
    .prop_filter("at most 63 characters", |s| s.len() <= 63)
}

proptest! {
    #[test]
    fn gw_01_any_subdomain_gives_a_stable_dns_1035_label(input in subdomain()) {
        let got = name(&input);
        prop_assert!(dns_1035(&got), "{got:?} is not a DNS-1035 label of at most 63 characters");
        prop_assert_eq!(&got, &name(&input));
    }

    #[test]
    fn gw_07_acceptance_bounds_for_any_gateway((gw, class, settings) in any_gateway()) {
        let got = gateway(&gw, &class, &settings);
        prop_assert_eq!(got.listeners.len(), gw.listeners.len());
        if got.is_accepted() {
            prop_assert!(gw.name.chars().count() <= 63);
            prop_assert!(class.name.chars().count() <= 63);
            let ports = got.ports();
            prop_assert!(ports.windows(2).all(|w| w[0] < w[1]), "ports {ports:?}");
            prop_assert!(ports.iter().all(|p| *p >= 1 && *p != ADMIN_PORT), "ports {ports:?}");
            prop_assert!(!ports.is_empty() && ports.len() <= MAX_LISTENERS);
            prop_assert!(got.replicas >= 1 && got.replicas <= settings.max_replicas);
        } else {
            prop_assert!(got.ports().is_empty());
        }
        prop_assert_eq!(&got, &gateway(&gw, &class, &settings));
    }
}
