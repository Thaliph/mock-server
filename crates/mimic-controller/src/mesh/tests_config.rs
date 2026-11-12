//! MSH-18: the mesh variables of the controller (spec §10.2).

use std::time::Duration;

use mimic_core::mesh::view::Limits;
use mimic_runtime::config::Env;

use super::MeshConfig;

fn parse(pairs: &[(&str, &str)]) -> Result<MeshConfig, String> {
    let env = Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    MeshConfig::from_env(&env).map_err(|e| e.to_string())
}

#[test]
fn msh_18_unset_variables_give_the_defaults() {
    let c = parse(&[]).expect("defaults");
    assert!(c.enabled);
    assert_eq!(c.cluster, "local");
    assert_eq!(c.watch_timeout, Duration::from_secs(30));
    assert_eq!(c.debounce, Duration::from_millis(100));
    assert_eq!(c.cache_bytes, 64 << 20);
    assert_eq!(c.max_watches, 10_000);
    assert_eq!(c.limits, Limits::default());
}

#[test]
fn msh_18_valid_values_are_read() {
    let c = parse(&[
        ("MIMIC_MESH", "off"),
        ("MIMIC_CLUSTER_NAME", "prod-eu1"),
        ("MIMIC_MESH_WATCH_TIMEOUT", "400ms"),
        ("MIMIC_MESH_DEBOUNCE", "50ms"),
        ("MIMIC_MESH_VIEW_CACHE_BYTES", "8Mi"),
        ("MIMIC_SIDECAR_API_MAX_WATCHES", "2"),
        ("MIMIC_MESH_MAX_SERVICES", "7"),
        ("MIMIC_MESH_MAX_ENDPOINTS", "9"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "64Ki"),
    ])
    .expect("valid");
    assert!(!c.enabled);
    assert_eq!(c.cluster, "prod-eu1");
    assert_eq!(c.watch_timeout, Duration::from_millis(400));
    assert_eq!(c.debounce, Duration::from_millis(50));
    assert_eq!(c.cache_bytes, 8 << 20);
    assert_eq!(c.max_watches, 2);
    assert_eq!(
        c.limits,
        Limits {
            max_services: 7,
            max_endpoints: 9,
            max_view_bytes: 64 << 10
        }
    );
}

#[test]
fn msh_18_an_invalid_value_is_refused_naming_the_variable() {
    let long = "a".repeat(64);
    let refused: &[(&str, &str)] = &[
        ("MIMIC_MESH", "maybe"),
        ("MIMIC_MESH", "ON"),
        ("MIMIC_CLUSTER_NAME", &long),
        ("MIMIC_CLUSTER_NAME", "Prod"),
        ("MIMIC_CLUSTER_NAME", "a_b"),
        ("MIMIC_CLUSTER_NAME", "-a"),
        ("MIMIC_CLUSTER_NAME", "a-"),
        ("MIMIC_CLUSTER_NAME", ""),
        ("MIMIC_MESH_WATCH_TIMEOUT", "50ms"),
        ("MIMIC_MESH_WATCH_TIMEOUT", "11m"),
        ("MIMIC_MESH_WATCH_TIMEOUT", "abc"),
        ("MIMIC_MESH_DEBOUNCE", "5ms"),
        ("MIMIC_MESH_DEBOUNCE", "2s"),
        ("MIMIC_MESH_VIEW_CACHE_BYTES", "1Mi"),
        ("MIMIC_MESH_VIEW_CACHE_BYTES", "5Gi"),
        ("MIMIC_SIDECAR_API_MAX_WATCHES", "0"),
        ("MIMIC_SIDECAR_API_MAX_WATCHES", "1000001"),
        ("MIMIC_MESH_MAX_SERVICES", "0"),
        ("MIMIC_MESH_MAX_SERVICES", "100001"),
        ("MIMIC_MESH_MAX_ENDPOINTS", "0"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "1Ki"),
        ("MIMIC_MESH_MAX_VIEW_BYTES", "512Mi"),
    ];
    for (var, value) in refused {
        let error = parse(&[(var, value)]).expect_err(&format!("{var}={value} was accepted"));
        assert!(error.contains(var), "{var}={value}: {error}");
    }
}
