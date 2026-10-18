//! Construction des requêtes de `mimicctl`, sans cluster (spec §9.5, CLI-*).

use mimicctl::{
    ModeOptions, Target, WorkloadData, check_namespace, export_path, operations_path,
    report_markdown, requests_path, sample_requests_path, status_table, workload_apply,
    workload_apply_with,
};
use serde_json::{Value, json};

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

fn op(direction: &str, authority: &str, method: &str, path: &str, count: u64) -> Value {
    json!({
        "direction": direction,
        "authority": authority,
        "method": method,
        "path": path,
        "count": count,
        "status": {"200": 0.75, "404": 0.25},
        "latency_ms": {"p50": 1.5, "p99": 12.0},
        "parameters": [],
    })
}

fn checkout() -> WorkloadData {
    // 12 opérations : 10 sur le service, 2 sur la dépendance quote:8080.
    let mut operations: Vec<Value> = (0..10)
        .map(|i| op("inbound", "checkout", "GET", &format!("/r{i}"), 10 + i))
        .collect();
    operations.push(op("inbound", "checkout", "GET", "/items/{id}", 500));
    let mut post = op("outbound", "quote:8080", "POST", "/getquote", 300);
    post["parameters"] = json!([
        {"name": "x-tenant", "in": "header", "required": true, "schema": {}},
        {"name": "page", "in": "query", "required": false, "schema": {}},
    ]);
    operations.push(post);
    let requests = vec![
        // Ne correspond pas : autre méthode.
        json!({"direction": "inbound", "authority": "checkout:8080", "method": "POST", "path": "/items/42",
               "req_headers": [{"name": "x-wrong", "value": "1"}], "instance": "checkout-a"}),
        json!({"direction": "inbound", "authority": "checkout:8080", "method": "GET", "path": "/items/42",
               "req_headers": [
                   {"name": "accept", "value": "application/json"},
                   {"name": "authorization", "value": "[REDACTED]"},
                   {"name": "x-pipe", "value": "a|b"},
                   {"name": "cookie", "value": "x".repeat(200)},
               ], "instance": "checkout-a"}),
    ];
    WorkloadData {
        namespace: "shop".into(),
        name: "checkout".into(),
        mode: Some("record".into()),
        pods: 2,
        aligned: 2,
        operations,
        requests,
    }
}

#[test]
fn cli_04_report_lists_apis_and_top_calls_with_headers() {
    let md = report_markdown(&[checkout()], 10);

    // API découvertes : le service et sa dépendance, opérations et appels.
    let service = md.lines().find(|l| l.contains("| service |")).expect(&md);
    assert!(
        service.contains("checkout") && service.contains("| 11 |"),
        "{service}"
    );
    assert!(
        service.contains(&(500 + (10..20).sum::<u64>()).to_string()),
        "{service}"
    );
    let dep = md.lines().find(|l| l.contains("`quote:8080`")).expect(&md);
    assert!(dep.contains("| 1 |") && dep.contains("| 300 |"), "{dep}");

    // Exactement 10 opérations, par appels décroissants.
    let rows: Vec<&str> = md
        .lines()
        .filter(|l| {
            l.starts_with("| ")
                && l.contains(" | `")
                && (l.contains(" | GET ") || l.contains(" | POST "))
        })
        .collect();
    assert_eq!(rows.len(), 10, "{md}");
    assert!(rows[0].contains("/items/{id}"), "{}", rows[0]);
    assert!(rows[1].contains("/getquote"), "{}", rows[1]);
    assert!(rows[2].contains("/r9"), "{}", rows[2]);
    assert!(!md.contains("`/r0`") && !md.contains("`/r1`"), "{md}");
    assert!(
        rows[0].contains("200 75 %") && rows[0].contains("404 25 %"),
        "{}",
        rows[0]
    );
    assert!(
        rows[0].contains("1.5") && rows[0].contains("12"),
        "{}",
        rows[0]
    );

    // Headers de la requête enregistrée correspondante.
    assert!(md.contains("| `accept` | `application/json` |"), "{md}");
    assert!(md.contains("| `authorization` | `[REDACTED]` |"), "{md}");
    assert!(md.contains("a\\|b"), "{md}");
    assert!(!md.contains("x-wrong"), "{md}");
    assert!(!md.contains(&"x".repeat(81)), "{md}");
    assert!(md.contains(&format!("{}…", "x".repeat(80))), "{md}");
    // Sans requête : les paramètres header déclarés.
    assert!(md.contains("`x-tenant`"), "{md}");
    assert!(!md.contains("`page`"), "{md}");

    // Déterministe.
    assert_eq!(md, report_markdown(&[checkout()], 10));
}

#[test]
fn report_without_traffic_says_so() {
    let mut w = checkout();
    w.operations.clear();
    w.requests.clear();
    let md = report_markdown(&[w], 10);
    assert!(md.contains("shop/checkout"), "{md}");
    assert!(md.contains("Aucun trafic"), "{md}");
    assert!(report_markdown(&[], 10).contains("Aucun workload"));
}

#[test]
fn report_paths() {
    assert_eq!(
        operations_path("shop", "checkout"),
        "/v1/workloads/shop/checkout/operations"
    );
    assert_eq!(
        sample_requests_path("shop", "checkout", "outbound", 500),
        "/v1/workloads/shop/checkout/requests?direction=outbound&format=json&limit=500"
    );
}

#[test]
fn cli_05_status_of_a_previous_generation_is_not_shown_as_aligned() {
    let mut mw = workload_apply("shop", "checkout", "mock", None).unwrap();
    // Le spec vient de passer en mock (génération 5) ; le statut date encore du record.
    mw.metadata.generation = Some(5);
    mw.status = Some(mimic_controller::crd::MimicWorkloadStatus {
        snapshot: Some("3f9a0c1e5b7d2a46".into()),
        pods: 2,
        aligned: 2,
        observed_generation: Some(4),
        ..Default::default()
    });
    let table = status_table(std::slice::from_ref(&mw));
    let row = table.lines().nth(1).unwrap();
    assert!(row.ends_with("?/2"), "{table}");
    assert!(!row.contains("3f9a0c1e5b7d2a46"), "{table}");

    mw.status.as_mut().unwrap().observed_generation = Some(5);
    let table = status_table(&[mw]);
    let row = table.lines().nth(1).unwrap();
    assert!(
        row.ends_with("2/2") && row.contains("3f9a0c1e5b7d2a46"),
        "{table}"
    );
}

#[test]
fn cli_06_mock_is_refused_in_a_production_namespace() {
    let prod: std::collections::BTreeMap<String, String> =
        [("env".to_string(), "production".to_string())].into();
    for mode in ["mock", "mock-partial"] {
        let err = check_namespace("shop", mode, Some(&prod)).unwrap_err();
        assert!(err.contains("shop"), "{err}");
        assert!(err.contains("env=production"), "{err}");
    }
    for mode in ["record", "passthrough"] {
        assert!(check_namespace("shop", mode, Some(&prod)).is_ok());
    }
    let staging = [("env".to_string(), "staging".to_string())].into();
    assert!(check_namespace("shop", "mock", Some(&staging)).is_ok());
    // Labels illisibles (droit absent) : le contrôleur tranche.
    assert!(check_namespace("shop", "mock", None).is_ok());
}

#[test]
fn cli_07_capture_inbound_option() {
    let opts = ModeOptions {
        capture_inbound: Some("metadata".into()),
        ..ModeOptions::default()
    };
    let mw = workload_apply_with("shop", "checkout", "mock", &opts).unwrap();
    let v = serde_json::to_value(&mw).unwrap();
    assert_eq!(
        v["spec"],
        json!({"mode": "mock", "capture": {"inbound": "metadata"}})
    );
    let bad = ModeOptions {
        capture_inbound: Some("fast".into()),
        ..ModeOptions::default()
    };
    let err = workload_apply_with("shop", "checkout", "mock", &bad).unwrap_err();
    for c in ["full", "metadata", "off"] {
        assert!(err.contains(c), "{err}");
    }
}

#[test]
fn cli_07_selector_option_and_refused_status() {
    let opts = ModeOptions {
        selector: Some("pod-template-hash=7d9f,track=green".into()),
        capture_inbound: Some("metadata".into()),
        ..ModeOptions::default()
    };
    let mw = workload_apply_with("shop", "checkout", "mock", &opts).unwrap();
    let v = serde_json::to_value(&mw).unwrap();
    assert_eq!(
        v["spec"],
        json!({
            "mode": "mock",
            "selector": {"matchLabels": {"pod-template-hash": "7d9f", "track": "green"}},
            "capture": {"inbound": "metadata"},
        })
    );
    for bad in ["track", "=green", "track=", "a=b,,c=d", "Track Name=x"] {
        let opts = ModeOptions {
            selector: Some(bad.into()),
            ..ModeOptions::default()
        };
        assert!(
            workload_apply_with("shop", "checkout", "mock", &opts).is_err(),
            "{bad}"
        );
    }

    // Statut d'une ressource refusée par le garde-fou production.
    let mut mw = workload_apply("shop", "checkout", "mock", None).unwrap();
    mw.status = Some(mimic_controller::crd::MimicWorkloadStatus {
        pods: 2,
        aligned: 2,
        conditions: vec![mimic_controller::crd::StatusCondition {
            type_: "Allowed".into(),
            status: "False".into(),
            reason: Some("ProductionNamespace".into()),
            message: None,
        }],
        ..Default::default()
    });
    let table = status_table(&[mw]);
    assert!(
        table.lines().nth(1).unwrap().contains("mock (refusé)"),
        "{table}"
    );
}

fn controller_pod_of(name: &str, phase: &str, leader: bool) -> k8s_openapi::api::core::v1::Pod {
    let mut labels = json!({"app.kubernetes.io/name": "mimic-controller"});
    if leader {
        labels["mimic.platform.io/leader"] = json!("true");
    }
    serde_json::from_value(json!({
        "metadata": {"name": name, "labels": labels},
        "status": {"phase": phase}
    }))
    .unwrap()
}

/// L'interface est lue sur la réplique active du contrôleur (CLI-08, spec §5.5.4, §9.5).
#[test]
fn cli_08_ui_is_read_on_the_active_replica() {
    use mimicctl::controller_pod;
    let passive = controller_pod_of("ctl-p", "Running", false);
    let active = controller_pod_of("ctl-a", "Running", true);
    assert_eq!(
        controller_pod(&[passive.clone(), active.clone()]).unwrap(),
        "ctl-a"
    );
    assert_eq!(
        controller_pod(&[active.clone(), passive.clone()]).unwrap(),
        "ctl-a"
    );
    // Marquée mais plus en marche (arrêt en cours) : la passive en marche.
    let leaving = controller_pod_of("ctl-a", "Failed", true);
    assert_eq!(
        controller_pod(&[leaving, passive.clone()]).unwrap(),
        "ctl-p"
    );
    // Contrôleur d'une version antérieure, sans marque : le premier pod en marche.
    let pending = controller_pod_of("ctl-0", "Pending", false);
    assert_eq!(
        controller_pod(&[pending.clone(), passive.clone()]).unwrap(),
        "ctl-p"
    );
    assert!(controller_pod(&[pending]).is_err());
    assert!(controller_pod(&[]).is_err());
}
