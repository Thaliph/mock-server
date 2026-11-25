//! GW-16: the `MIMIC_GATEWAY_*` variables of the controller (spec §10.2).

use std::collections::BTreeSet;

use mimic_core::gateway::{ServiceType, Settings};
use mimic_runtime::config::Env;
use serde_json::json;

use super::config::from_env;

fn parse(pairs: &[(&str, &str)], mesh: bool) -> Result<Option<Settings>, String> {
    let env = Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    from_env(&env, mesh, "mimic-sidecar:test", "debug").map_err(|e| e.to_string())
}

#[test]
fn gw_16_each_variable_is_checked() {
    // Off: no namespace.
    assert_eq!(parse(&[], true), Ok(None));
    assert_eq!(parse(&[("MIMIC_GATEWAY_NAMESPACES", "")], true), Ok(None));
    assert_eq!(parse(&[("MIMIC_GATEWAY_NAMESPACES", "")], false), Ok(None));

    // The defaults of §10.2.
    let settings = parse(&[("MIMIC_GATEWAY_NAMESPACES", "shop,edge")], true)
        .expect("valid")
        .expect("on");
    assert_eq!(
        settings,
        Settings {
            namespaces: BTreeSet::from(["edge".to_string(), "shop".to_string()]),
            max_replicas: 10,
            service_type: ServiceType::ClusterIP,
            image: "mimic-sidecar:test".into(),
            log: "debug".into(),
            resources: json!({"requests": {"cpu": "100m", "memory": "64Mi"},
                              "limits": {"memory": "128Mi"}}),
        }
    );

    // Values that are read.
    let on = |var: &str, value: &str| {
        parse(&[("MIMIC_GATEWAY_NAMESPACES", "shop"), (var, value)], true)
            .unwrap_or_else(|e| panic!("{var}={value}: {e}"))
            .expect("on")
    };
    assert_eq!(on("MIMIC_GATEWAY_MAX_REPLICAS", "1").max_replicas, 1);
    assert_eq!(on("MIMIC_GATEWAY_MAX_REPLICAS", "100").max_replicas, 100);
    assert_eq!(
        on("MIMIC_GATEWAY_SERVICE_TYPE", "NodePort").service_type,
        ServiceType::NodePort
    );
    assert_eq!(
        on("MIMIC_GATEWAY_SERVICE_TYPE", "LoadBalancer").service_type,
        ServiceType::LoadBalancer
    );
    assert_eq!(
        on(
            "MIMIC_GATEWAY_RESOURCES",
            r#"{"limits":{"cpu":"1500m","memory":"1Gi"}}"#
        )
        .resources,
        json!({"limits": {"cpu": "1500m", "memory": "1Gi"}})
    );
    assert_eq!(
        on(
            "MIMIC_GATEWAY_RESOURCES",
            r#"{"requests":{"cpu":"0.5","memory":"1e3"}}"#
        )
        .resources,
        json!({"requests": {"cpu": "0.5", "memory": "1e3"}})
    );
    let mut many: Vec<String> = (0..256).map(|i| format!("ns-{i}")).collect();
    assert_eq!(
        on("MIMIC_GATEWAY_NAMESPACES", &many.join(","))
            .namespaces
            .len(),
        256
    );

    // Refused, naming the variable.
    many.push("ns-256".into());
    let long = "a".repeat(64);
    let refused: Vec<(&str, &str)> = vec![
        ("MIMIC_GATEWAY_NAMESPACES", "Shop"),
        ("MIMIC_GATEWAY_NAMESPACES", "a_b"),
        ("MIMIC_GATEWAY_NAMESPACES", "-a"),
        ("MIMIC_GATEWAY_NAMESPACES", "a-"),
        ("MIMIC_GATEWAY_NAMESPACES", &long),
        ("MIMIC_GATEWAY_NAMESPACES", "a,,b"),
        ("MIMIC_GATEWAY_NAMESPACES", "shop,"),
        ("MIMIC_GATEWAY_NAMESPACES", "shop,shop"),
        ("MIMIC_GATEWAY_NAMESPACES", "shop,edge,shop"),
        ("MIMIC_GATEWAY_SERVICE_TYPE", "ExternalName"),
        ("MIMIC_GATEWAY_SERVICE_TYPE", "clusterip"),
        ("MIMIC_GATEWAY_SERVICE_TYPE", ""),
        ("MIMIC_GATEWAY_MAX_REPLICAS", "0"),
        ("MIMIC_GATEWAY_MAX_REPLICAS", "101"),
        ("MIMIC_GATEWAY_MAX_REPLICAS", "two"),
        ("MIMIC_GATEWAY_RESOURCES", "not json"),
        ("MIMIC_GATEWAY_RESOURCES", "[]"),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"cpu":"1"}"#),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"limits":{"cpu":"x"}}"#),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"limits":{"cpu":1}}"#),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"limits":[]}"#),
        (
            "MIMIC_GATEWAY_RESOURCES",
            r#"{"requests":{"memory":"1Mx"}}"#,
        ),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"requests":{"memory":""}}"#),
        ("MIMIC_GATEWAY_RESOURCES", r#"{"claims":[{"name":"a"}]}"#),
    ];
    let joined = many.join(",");
    for (var, value) in refused
        .into_iter()
        .chain([("MIMIC_GATEWAY_NAMESPACES", &*joined)])
    {
        let pairs = if var == "MIMIC_GATEWAY_NAMESPACES" {
            vec![(var, value)]
        } else {
            vec![("MIMIC_GATEWAY_NAMESPACES", "shop"), (var, value)]
        };
        let error = parse(&pairs, true).expect_err(var);
        assert!(error.contains(var), "{var}={value:.40}: {error}");
    }

    // The namespaces need the mesh.
    let error = parse(&[("MIMIC_GATEWAY_NAMESPACES", "shop")], false).expect_err("mesh off");
    assert!(
        error.contains("MIMIC_GATEWAY_NAMESPACES") && error.contains("MIMIC_MESH=on"),
        "{error}"
    );
}

#[test]
fn gw_16_the_other_variables_are_checked_while_the_gateway_is_off() {
    for (var, value) in [
        ("MIMIC_GATEWAY_SERVICE_TYPE", "ExternalName"),
        ("MIMIC_GATEWAY_MAX_REPLICAS", "0"),
        ("MIMIC_GATEWAY_RESOURCES", "[]"),
    ] {
        let error = parse(&[(var, value)], false).expect_err(var);
        assert!(error.contains(var), "{var}: {error}");
    }
}
