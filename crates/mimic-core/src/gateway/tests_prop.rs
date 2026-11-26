//! GW-01 and GW-07 properties: generated names are valid and stable; the acceptance and the
//! objects of any converted Gateway keep their bounds and do not depend on the listener order.

use proptest::prelude::*;

use super::accept::gateway;
use super::objects::{name, objects};
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

/// Deterministic shuffle: sorts by a mixed key, so every seed is one permutation.
fn shuffle<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mix = |i: usize| {
        let mut x = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 31;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^ (x >> 29)
    };
    let mut keyed: Vec<(u64, usize)> = (0..items.len()).map(|i| (mix(i), i)).collect();
    keyed.sort();
    keyed.into_iter().map(|(_, i)| items[i].clone()).collect()
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

    #[test]
    fn gw_07_objects_for_any_gateway((gw, class, settings) in any_gateway(), seed in any::<u64>()) {
        let acceptance = gateway(&gw, &class, &settings);
        let got = objects(&gw, &acceptance, &settings);
        prop_assert_eq!(got.is_some(), acceptance.is_accepted());
        prop_assert_eq!(&got, &objects(&gw, &acceptance, &settings));
        if let Some(o) = got {
            let all = [&o.deployment, &o.service, &o.pod_disruption_budget];
            let name = &o.deployment["metadata"]["name"];
            prop_assert!(name.as_str().is_some_and(dns_1035), "name {name}");
            for object in all {
                prop_assert_eq!(&object["metadata"]["name"], name);
                let labels = object["metadata"]["labels"].as_object().cloned().unwrap_or_default();
                prop_assert_eq!(labels.len(), 5);
                for (key, value) in &labels {
                    prop_assert!(value.as_str().is_some_and(|v| v.len() <= 63), "label {key}");
                }
            }
            let container = &o.deployment["spec"]["template"]["spec"]["containers"][0];
            let ports: Vec<u64> = container["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p.get("name").is_none())
                .filter_map(|p| p["containerPort"].as_u64())
                .collect();
            prop_assert!(ports.windows(2).all(|w| w[0] < w[1]), "ports {ports:?}");
            prop_assert!(ports.iter().all(|p| (1..=65535).contains(p) && *p != 15100));
            let service_ports: Vec<u64> = o.service["spec"]["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["port"].as_u64())
                .collect();
            prop_assert_eq!(&service_ports, &ports);
            for e in container["env"].as_array().into_iter().flatten() {
                let n = e["name"].as_str().unwrap_or_default();
                prop_assert!(["MIMIC_GATEWAY_PORTS", "MIMIC_LOG", "MIMIC_MAX_CONNECTIONS", "MIMIC_ROLE"].contains(&n), "env {n}");
            }
            let mut shuffled = gw.clone();
            shuffled.listeners = shuffle(&gw.listeners, seed);
            let again = objects(&shuffled, &gateway(&shuffled, &class, &settings), &settings);
            prop_assert!(again.is_some());
            if let Some(again) = again {
                prop_assert_eq!(o.deployment.to_string(), again.deployment.to_string());
                prop_assert_eq!(o.service.to_string(), again.service.to_string());
                prop_assert_eq!(o.pod_disruption_budget.to_string(), again.pod_disruption_budget.to_string());
            }
        }
    }
}
