//! Annotation `mimic.platform.io/cni-rules` lue par le plugin CNI, en root sur le nœud
//! (`mimic_cni::settings`) : toute variable inconnue est refusée, et `MIMIC_IPTABLES` ne
//! désigne qu'un des trois binaires admis (CNI-01).
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use mimic_init::IPTABLES_TOOLS;

const KNOWN: [&str; 6] = [
    "MIMIC_INBOUND_PORTS",
    "MIMIC_EXCLUDE_OUTBOUND_CIDRS",
    "MIMIC_EXCLUDE_OUTBOUND_PORTS",
    "MIMIC_MESH",
    "MIMIC_IPV6",
    "MIMIC_IPTABLES",
];

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let got = mimic_cni::settings(text, Some("10.0.0.7".into()));
    let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(text) else {
        assert!(got.is_err(), "annotation illisible acceptée : {text:?}");
        return;
    };
    let unknown = map.keys().find(|k| !KNOWN.contains(&k.as_str()));
    let bad_tool = map
        .get("MIMIC_IPTABLES")
        .filter(|t| !IPTABLES_TOOLS.contains(&t.as_str()));
    match got {
        Ok(s) => {
            assert!(
                unknown.is_none(),
                "variable inconnue acceptée : {unknown:?}"
            );
            assert!(bad_tool.is_none(), "binaire refusé accepté : {bad_tool:?}");
            assert!(
                IPTABLES_TOOLS.contains(&s.iptables.as_str()),
                "{:?}",
                s.iptables
            );
            assert_eq!(s.pod_ip.as_deref(), Some("10.0.0.7"));
            let field = |k: &str, v: &str| map.get(k).is_none_or(|m| m == v);
            assert!(field("MIMIC_INBOUND_PORTS", &s.inbound_ports));
            assert!(field(
                "MIMIC_EXCLUDE_OUTBOUND_CIDRS",
                &s.exclude_outbound_cidrs
            ));
            assert!(field(
                "MIMIC_EXCLUDE_OUTBOUND_PORTS",
                &s.exclude_outbound_ports
            ));
            assert!(field("MIMIC_MESH", &s.mesh));
            assert_eq!(s.ipv6, map.get("MIMIC_IPV6").is_some_and(|v| v == "true"));
        }
        Err(_) => assert!(
            unknown.is_some() || bad_tool.is_some(),
            "annotation valide refusée : {text:?}"
        ),
    }
});
