//! Construction des requêtes de `mimicctl`, sans cluster (spec §9.5, CLI-*).

use mimicctl::{Target, export_path, requests_path, status_table, workload_apply};

#[test]
fn cli_01_mode_builds_an_apply_of_the_workload_resource() {
    let mw = workload_apply("shop", "checkout", "mock", None).unwrap();
    let v = serde_json::to_value(&mw).unwrap();
    assert_eq!(v["apiVersion"], "mimic.platform.io/v1alpha1");
    assert_eq!(v["kind"], "MimicWorkload");
    assert_eq!(v["metadata"]["namespace"], "shop");
    assert_eq!(v["metadata"]["name"], "checkout");
    assert_eq!(v["spec"], serde_json::json!({"mode": "mock"}));
    assert!(v.get("status").is_none_or(serde_json::Value::is_null));

    let mw = workload_apply("shop", "checkout", "mock", Some("3f9a0c1e5b7d2a46")).unwrap();
    assert_eq!(mw.spec.snapshot.as_deref(), Some("3f9a0c1e5b7d2a46"));
}

#[test]
fn cli_02_unknown_mode_is_refused_with_the_valid_modes() {
    let err = workload_apply("shop", "checkout", "fast", None).unwrap_err();
    for m in ["passthrough", "record", "mock", "mock-partial"] {
        assert!(err.contains(m), "{err}");
    }
    assert!(workload_apply("shop", "Checkout_", "mock", None).is_err());
}

#[test]
fn cli_03_controller_paths() {
    assert_eq!(
        export_path(
            "shop",
            "checkout",
            &Target::Outbound("api.stripe.com".into())
        ),
        "/v1/workloads/shop/checkout/openapi/outbound/api.stripe.com"
    );
    assert_eq!(
        export_path("shop", "checkout", &Target::Inbound),
        "/v1/workloads/shop/checkout/openapi/inbound"
    );
    assert_eq!(
        requests_path("shop", "checkout", "k6", 100),
        "/v1/workloads/shop/checkout/requests?direction=inbound&format=k6&limit=100"
    );
}

#[test]
fn status_lists_mode_snapshot_and_alignment() {
    let mut mw = workload_apply("shop", "checkout", "mock", None).unwrap();
    mw.status = Some(mimic_controller::crd::MimicWorkloadStatus {
        snapshot: Some("3f9a0c1e5b7d2a46".into()),
        pods: 3,
        aligned: 2,
        ..Default::default()
    });
    let table = status_table(&[mw]);
    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("NAMESPACE"));
    for cell in ["shop", "checkout", "mock", "3f9a0c1e5b7d2a46", "2/3"] {
        assert!(lines[1].contains(cell), "{table}");
    }
}
