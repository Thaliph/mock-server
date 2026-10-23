//! Construction des requêtes de `mimicctl`, sans cluster (spec §9.5, CLI-*).

use mimicctl::{
    ModeOptions, Target, WorkloadData, check_namespace, export_path, operations_path,
    outbound_authorities, report_markdown, requests_path, resolve_authority, sample_requests_path,
    snapshot_export_path, snapshot_gc_deleted, snapshot_gc_path, snapshot_pin_body,
    snapshot_pin_path, snapshot_table, snapshots_path, status_table, workload_apply,
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
        ..WorkloadData::default()
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
fn cli_08_mock_websocket_option() {
    let opts = ModeOptions {
        mock_websocket: Some("refuse".into()),
        ..ModeOptions::default()
    };
    let mw = workload_apply_with("shop", "checkout", "mock", &opts).unwrap();
    let v = serde_json::to_value(&mw).unwrap();
    assert_eq!(
        v["spec"],
        json!({"mode": "mock", "mock": {"websocket": "refuse"}})
    );
    let bad = ModeOptions {
        mock_websocket: Some("drop".into()),
        ..ModeOptions::default()
    };
    let err = workload_apply_with("shop", "checkout", "mock", &bad).unwrap_err();
    for w in ["relay", "refuse"] {
        assert!(err.contains(w), "{err}");
    }
    // Sans l'option, pas de spec.mock : l'apply la retire.
    let mw = workload_apply_with("shop", "checkout", "mock", &ModeOptions::default()).unwrap();
    let v = serde_json::to_value(&mw).unwrap();
    assert!(v["spec"].get("mock").is_none(), "{v}");
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

/// L'interface est lue sur la réplique active du contrôleur (CLI-09, spec §5.5.4, §9.5).
#[test]
fn cli_09_ui_is_read_on_the_active_replica() {
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

/// `--outbound` : `hôte:port`, ou l'hôte seul s'il n'a qu'une authority connue (CLI-10).
#[test]
fn cli_10_outbound_host_without_port_is_resolved() {
    let ops = vec![
        op("outbound", "quote:9090", "GET", "/q", 1),
        op("outbound", "api.stripe.com:443", "POST", "/v1/charges", 3),
        op("outbound", "quote:8080", "POST", "/getquote", 2),
        op("outbound", "quote:8080", "GET", "/health", 2),
        op("inbound", "checkout", "GET", "/items/{id}", 9),
    ];
    let known = outbound_authorities(&ops);
    assert_eq!(known, ["api.stripe.com:443", "quote:8080", "quote:9090"]);
    assert_eq!(
        resolve_authority("api.stripe.com:443", &known).unwrap(),
        "api.stripe.com:443"
    );
    assert_eq!(
        resolve_authority("api.stripe.com", &known).unwrap(),
        "api.stripe.com:443"
    );
    let err = resolve_authority("quote", &known).unwrap_err();
    assert!(
        err.contains("quote:8080") && err.contains("quote:9090"),
        "{err}"
    );
    assert!(!err.contains("api.stripe.com"), "{err}");
    let err = resolve_authority("inconnu.io", &known).unwrap_err();
    for a in &known {
        assert!(err.contains(a.as_str()), "{err}");
    }
    let err = resolve_authority("quote:7070", &known).unwrap_err();
    assert!(err.contains("quote:8080"), "{err}");
    assert!(resolve_authority("api.stripe.com", &[]).is_err());

    // L'aide cite un exemple hôte:port.
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_mimicctl"))
        .args(["export", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("api.stripe.com:443"), "{help}");
}

fn unmatched_entry(method: &str, path: &str, reason: &str, count: u64, last: u64) -> Value {
    json!({
        "authority": "api.saas.io:443", "method": method, "path": path, "reason": reason,
        "count": count, "first_ms": 0, "last_ms": last,
    })
}

/// Requêtes sans réponse du mock dans `status` et `report` (CLI-11).
#[test]
fn cli_11_status_and_report_show_unmatched_requests() {
    let mut mw = workload_apply("shop", "checkout", "mock", None).unwrap();
    mw.status = Some(mimic_controller::crd::MimicWorkloadStatus {
        pods: 2,
        aligned: 2,
        unmatched: 7,
        ..Default::default()
    });
    let table = status_table(std::slice::from_ref(&mw));
    let lines: Vec<&str> = table.lines().collect();
    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(
        header,
        [
            "NAMESPACE",
            "WORKLOAD",
            "MODE",
            "SNAPSHOT",
            "UNMATCHED",
            "ALIGNED"
        ]
    );
    let row: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(row[4], "7", "{table}");
    assert_eq!(row[5], "2/2", "{table}");
    // Statut publié par un contrôleur d'une version antérieure : pas de champ.
    let old: mimic_controller::crd::MimicWorkloadStatus =
        serde_json::from_value(json!({"pods": 1, "aligned": 1})).unwrap();
    assert_eq!(old.unmatched, 0);
    mw.status = Some(old);
    let table = status_table(&[mw]);
    let row: Vec<&str> = table.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row[4], "0", "{table}");

    let mut w = checkout();
    w.unmatched = serde_json::from_value(json!({
        "total": 9,
        "entries": [
            unmatched_entry("GET", "/users/{int}", "unmatched", 8, 1_792_677_787_000),
            unmatched_entry("POST", "/upload", "too_large", 1, 1_792_677_000_000),
        ],
    }))
    .unwrap();
    let md = report_markdown(std::slice::from_ref(&w), 10);
    assert!(md.contains("| Sans réponse |"), "{md}");
    let summary = md
        .lines()
        .find(|l| l.contains("[shop/checkout]"))
        .expect(&md);
    assert!(summary.ends_with("| 9 |"), "{summary}");
    let section = md
        .split("### Requêtes sans réponse du mock")
        .nth(1)
        .expect(&md);
    let rows: Vec<&str> = section
        .lines()
        .filter(|l| l.starts_with("| GET") || l.starts_with("| POST"))
        .collect();
    assert_eq!(rows.len(), 2, "{section}");
    for cell in [
        "`api.saas.io:443`",
        "`/users/{int}`",
        "599",
        "| 8 |",
        "2026-10-22T14:03:07Z",
    ] {
        assert!(rows[0].contains(cell), "{cell} : {}", rows[0]);
    }
    assert!(
        rows[1].contains("`/upload`") && rows[1].contains("413"),
        "{}",
        rows[1]
    );
    assert_eq!(md, report_markdown(&[w], 10), "déterministe");

    // Sans relevé (aucune requête, ou contrôleur antérieur) : la section le dit.
    let md = report_markdown(&[checkout()], 10);
    let section = md
        .split("### Requêtes sans réponse du mock")
        .nth(1)
        .expect(&md);
    assert!(section.contains("Aucune requête sans réponse"), "{section}");
    let summary = md
        .lines()
        .find(|l| l.contains("[shop/checkout]"))
        .expect(&md);
    assert!(summary.ends_with("| 0 |"), "{summary}");
}

/// Corps d'une énumération ou structure `name {`, par comptage d'accolades (robuste à ce
/// qu'il y ait, ou pas, autre chose entre deux définitions).
fn brace_body<'a>(src: &'a str, marker: &str) -> &'a str {
    let start = src
        .find(marker)
        .unwrap_or_else(|| panic!("{marker} absent"))
        + marker.len();
    let mut depth = 1i32;
    for (i, c) in src[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[start..start + i];
                }
            }
            _ => {}
        }
    }
    panic!("accolade fermante de {marker} absente")
}

/// Variantes d'une énumération de sous-commandes clap, en kebab-case ; `prefix` compose un
/// nom pour une sous-commande imbriquée (`snapshot ls`).
fn variants(body: &str, prefix: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| {
            let name = l.trim();
            let name = name
                .strip_suffix(" {")
                .or_else(|| name.strip_suffix("(Box<LoadArgs>),"))?;
            (!name.starts_with(['/', '#'])).then(|| format!("{prefix}{}", kebab(name)))
        })
        .collect()
}

/// Sous-commandes et options longues déclarées dans `main.rs`, lues dans la source :
/// clap les construit par `derive`, les champs et les `#[arg(long …)]` sont donc la
/// référence. `Command::Snapshot` imbrique `SnapshotCommand` : ses variantes sont
/// documentées comme `snapshot <variante>`, pas comme une sous-commande à part.
fn declared() -> (Vec<String>, Vec<String>) {
    let src = include_str!("../src/main.rs");

    // `Snapshot(SnapshotArgs),` ne correspond à aucun des deux suffixes reconnus : sa
    // propre variante n'est jamais comptée, seules celles de `SnapshotCommand` le sont.
    let mut subcommands = variants(brace_body(src, "enum Command {"), "");
    subcommands.extend(variants(
        brace_body(src, "enum SnapshotCommand {"),
        "snapshot ",
    ));

    // Options longues : `#[arg(long)]` prend le nom du champ qui suit, `long = "x"` le sien.
    let mut longs = Vec::new();
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(arg) = line.trim().strip_prefix("#[arg(") else {
            continue;
        };
        if let Some(rest) = arg.split_once("long = \"") {
            longs.push(rest.1.split('"').next().unwrap().to_string());
            continue;
        }
        if !arg.split(['(', ')', ',', ' ']).any(|t| t == "long") {
            continue;
        }
        let field = lines
            .peek()
            .and_then(|l| l.trim().split_once(':'))
            .expect("champ après #[arg(long)]")
            .0;
        longs.push(kebab(field));
    }
    longs.sort();
    longs.dedup();
    (subcommands, longs)
}

fn kebab(ident: &str) -> String {
    let mut out = String::new();
    for c in ident.trim().chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(if c == '_' { '-' } else { c });
        }
    }
    out
}

/// La page `mimicctl` du guide documente chaque sous-commande et chaque option longue, et
/// c'est la seule référence : `docs/cli.md` a été fusionnée dedans (DOC-07).
#[test]
fn doc_07_every_subcommand_and_option_is_documented() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(
        !root.join("docs/cli.md").exists(),
        "docs/cli.md fait doublon avec docs/guide/CLI-mimicctl.md"
    );
    let page = std::fs::read_to_string(root.join("docs/guide/CLI-mimicctl.md")).unwrap();

    let (subcommands, longs) = declared();
    assert!(subcommands.len() >= 7, "{subcommands:?}");
    assert!(longs.len() >= 25, "{longs:?}");
    for name in &subcommands {
        assert!(
            page.contains(&format!("## `{name}`")),
            "sous-commande {name} sans section dans docs/guide/CLI-mimicctl.md"
        );
    }
    for long in &longs {
        assert!(
            page.contains(&format!("--{long}")),
            "option --{long} absente de docs/guide/CLI-mimicctl.md"
        );
    }
}

// Cycle de vie des snapshots (spec §5.6, §9.5, point 5) : `mimicctl snapshot ls|pin|export|import|gc`.

fn snap(id: &str, created_ms: i64, exchanges: u64, pinned: bool) -> Value {
    json!({"id": id, "size": 42, "modified_ms": created_ms, "created_ms": created_ms, "exchanges": exchanges, "pinned": pinned})
}

/// `ls` : tableau du plus récent au plus ancien, `REFERENCED` d'après le snapshot courant,
/// `PINNED` d'après le listing (CLI-12).
#[test]
fn cli_12_snapshot_ls_table() {
    let snaps = vec![snap("b2", 20, 3, true), snap("a1", 10, 1, false)];
    let table = snapshot_table(&snaps, Some("a1"));
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["ID", "CREATED_MS", "EXCHANGES", "PINNED", "REFERENCED"]
    );
    let row = |id: &str| {
        lines
            .iter()
            .find(|l| l.starts_with(id))
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .unwrap()
    };
    assert_eq!(row("b2"), ["b2", "20", "3", "true", "false"]);
    assert_eq!(row("a1"), ["a1", "10", "1", "false", "true"]);

    // Métadonnées inconnues (contrôleur antérieur ou snapshot sans métadonnées) : `?`.
    let unknown = vec![
        json!({"id": "x", "size": 1, "modified_ms": 1, "created_ms": null, "exchanges": null, "pinned": false}),
    ];
    let table = snapshot_table(&unknown, None);
    assert_eq!(
        table
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["x", "?", "?", "false", "false"]
    );
}

/// `pin`/`--unpin` : requête construite avant tout appel ; identifiant invalide refusé
/// (CLI-13).
#[test]
fn cli_13_snapshot_pin_path_and_body() {
    assert_eq!(
        snapshot_pin_path("shop", "checkout", "abc123").unwrap(),
        "/v1/workloads/shop/checkout/snapshots/abc123/pin"
    );
    assert_eq!(snapshot_pin_body(true), json!({"pinned": true}));
    assert_eq!(snapshot_pin_body(false), json!({"pinned": false}));

    let err = snapshot_pin_path("shop", "checkout", "../x").unwrap_err();
    assert!(err.contains("../x"), "{err}");
    assert!(snapshot_pin_path("shop", "checkout", "").is_err());
}

/// `export` : même route que la lecture d'un snapshot complet ; identifiant invalide
/// refusé avant tout appel (CLI-14).
#[test]
fn cli_14_snapshot_export_path() {
    assert_eq!(
        snapshot_export_path("shop", "checkout", "abc123").unwrap(),
        "/v1/workloads/shop/checkout/snapshots/abc123"
    );
    assert!(snapshot_export_path("shop", "checkout", "/abs").is_err());
}

/// `ls` sans identifiant (import s'appuie sur le même chemin que la liste, en `POST`) :
/// vérifie le chemin de la collection, utilisé aussi pour l'import (CLI-15).
#[test]
fn cli_15_snapshots_collection_path_is_shared_by_ls_and_import() {
    assert_eq!(
        snapshots_path("shop", "checkout"),
        "/v1/workloads/shop/checkout/snapshots"
    );
}

/// `gc` : chemin de la purge immédiate, et lecture des identifiants supprimés, triés
/// (CLI-16).
#[test]
fn cli_16_snapshot_gc_path_and_report() {
    assert_eq!(
        snapshot_gc_path("shop", "checkout"),
        "/v1/workloads/shop/checkout/snapshots/gc"
    );
    assert_eq!(
        snapshot_gc_deleted(&json!({"deleted": ["b", "a"]})),
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(
        snapshot_gc_deleted(&json!({"deleted": []})),
        Vec::<String>::new()
    );
}
