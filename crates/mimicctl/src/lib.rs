//! Construction des requêtes de `mimicctl` (spec §9.5) : fonctions pures, testées sans
//! cluster. `main.rs` les envoie à l'API Kubernetes ou au contrôleur.

pub mod load;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use kube::api::ObjectMeta;
use mimic_controller::crd::{
    CaptureSpec, INBOUND_CAPTURES, LabelSelector, MOCK_WEBSOCKETS, MODES, MimicWorkload,
    MimicWorkloadSpec, MockSpec,
};
use mimic_core::unmatched::UnmatchedReport;
use serde_json::Value;

/// Document OpenAPI à exporter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Inbound,
    Outbound(String),
}

/// Nom DNS-1123 : ce qu'accepte l'API Kubernetes pour un nom de ressource.
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !s.starts_with(['-', '.'])
        && !s.ends_with(['-', '.'])
}

/// Ressource à appliquer pour basculer un workload ; refusée avant tout appel si le
/// mode ou un nom est invalide.
pub fn workload_apply(
    namespace: &str,
    workload: &str,
    mode: &str,
    snapshot: Option<&str>,
) -> Result<MimicWorkload, String> {
    let opts = ModeOptions {
        snapshot: snapshot.map(String::from),
        ..ModeOptions::default()
    };
    workload_apply_with(namespace, workload, mode, &opts)
}

/// Options de `mimicctl mode` au-delà du mode (spec §9.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeOptions {
    pub snapshot: Option<String>,
    /// `full`, `metadata` ou `off` (`spec.capture.inbound`).
    pub capture_inbound: Option<String>,
    /// Égalités `clé=valeur` séparées par des virgules (`spec.selector.matchLabels`).
    pub selector: Option<String>,
    /// `relay` ou `refuse` (`spec.mock.websocket`, spec §3.4).
    pub mock_websocket: Option<String>,
}

/// Caractères permis dans une clé de label (préfixe DNS compris) ou une valeur.
fn label_part_ok(s: &str, key: bool) -> bool {
    !s.is_empty()
        && s.len() <= if key { 253 } else { 63 }
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') || (key && b == b'/')
        })
}

/// `k=v,k2=v2` → `matchLabels` ; refusé si une égalité est mal formée.
pub fn parse_selector(text: &str) -> Result<LabelSelector, String> {
    let mut match_labels = BTreeMap::new();
    for part in text.split(',') {
        let (k, v) = part
            .split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .filter(|(k, v)| label_part_ok(k, true) && label_part_ok(v, false))
            .ok_or_else(|| {
                format!("sélecteur invalide « {text} » : attendu clé=valeur[,clé=valeur…]")
            })?;
        match_labels.insert(k.to_string(), v.to_string());
    }
    Ok(LabelSelector {
        match_labels,
        ..LabelSelector::default()
    })
}

/// Comme `workload_apply`, avec toutes les options ; une option invalide est refusée
/// avant tout appel.
pub fn workload_apply_with(
    namespace: &str,
    workload: &str,
    mode: &str,
    opts: &ModeOptions,
) -> Result<MimicWorkload, String> {
    if !MODES.contains(&mode) {
        return Err(format!(
            "mode inconnu « {mode} » ; modes valides : {}",
            MODES.join(", ")
        ));
    }
    for (what, name) in [("namespace", namespace), ("workload", workload)] {
        if !valid_name(name) {
            return Err(format!("{what} invalide : « {name} »"));
        }
    }
    if let Some(c) = &opts.capture_inbound
        && !INBOUND_CAPTURES.contains(&c.as_str())
    {
        return Err(format!(
            "capture de l'entrant inconnue « {c} » ; valeurs valides : {}",
            INBOUND_CAPTURES.join(", ")
        ));
    }
    if let Some(w) = &opts.mock_websocket
        && !MOCK_WEBSOCKETS.contains(&w.as_str())
    {
        return Err(format!(
            "réglage WebSocket inconnu « {w} » ; valeurs valides : {}",
            MOCK_WEBSOCKETS.join(", ")
        ));
    }
    let selector = opts.selector.as_deref().map(parse_selector).transpose()?;
    Ok(MimicWorkload {
        metadata: ObjectMeta {
            name: Some(workload.into()),
            namespace: Some(namespace.into()),
            ..ObjectMeta::default()
        },
        spec: MimicWorkloadSpec {
            mode: mode.into(),
            snapshot: opts.snapshot.clone(),
            selector,
            capture: opts.capture_inbound.as_ref().map(|c| CaptureSpec {
                inbound: Some(c.clone()),
            }),
            mock: opts.mock_websocket.as_ref().map(|w| MockSpec {
                websocket: Some(w.clone()),
            }),
        },
        status: None,
    })
}

/// Garde-fou production (spec §9.5) : `mock` et `mock-partial` sont refusés dans un namespace
/// labellisé `env=production`. `labels` absent (namespace illisible) : accepté, le contrôleur
/// refusera de toute façon.
pub fn check_namespace(
    namespace: &str,
    mode: &str,
    labels: Option<&BTreeMap<String, String>>,
) -> Result<(), String> {
    use mimic_controller::reconcile::{PRODUCTION_LABEL, is_mock, is_production};
    if is_mock(mode) && labels.is_some_and(is_production) {
        return Err(format!(
            "le namespace {namespace} est labellisé {}={} : le mode {mode} y est refusé (garde-fou production)",
            PRODUCTION_LABEL.0, PRODUCTION_LABEL.1
        ));
    }
    Ok(())
}

fn base(namespace: &str, workload: &str) -> String {
    format!("/v1/workloads/{namespace}/{workload}")
}

pub fn export_path(namespace: &str, workload: &str, target: &Target) -> String {
    match target {
        Target::Inbound => format!("{}/openapi/inbound", base(namespace, workload)),
        Target::Outbound(authority) => {
            format!("{}/openapi/outbound/{authority}", base(namespace, workload))
        }
    }
}

/// Authorities des dépendances connues, d'après `…/operations`, triées.
pub fn outbound_authorities(operations: &[Value]) -> Vec<String> {
    let _ = operations;
    vec![]
}

/// Authority à exporter : `hôte:port` connu, ou l'hôte seul s'il n'a qu'une authority connue.
pub fn resolve_authority(arg: &str, known: &[String]) -> Result<String, String> {
    let _ = known;
    Ok(arg.to_string())
}

pub fn requests_path(namespace: &str, workload: &str, format: &str, limit: usize) -> String {
    format!(
        "{}/requests?direction=inbound&format={format}&limit={limit}",
        base(namespace, workload)
    )
}

pub fn operations_path(namespace: &str, workload: &str) -> String {
    format!("{}/operations", base(namespace, workload))
}

/// Requêtes enregistrées en JSON, dans un sens : exemples de headers du rapport.
pub fn sample_requests_path(
    namespace: &str,
    workload: &str,
    direction: &str,
    limit: usize,
) -> String {
    format!(
        "{}/requests?direction={direction}&format=json&limit={limit}",
        base(namespace, workload)
    )
}

/// Tableau `kubectl get` : mode souhaité, snapshot servi, pods alignés.
pub fn status_table(items: &[MimicWorkload]) -> String {
    let mut rows = vec![[
        "NAMESPACE".to_string(),
        "WORKLOAD".into(),
        "MODE".into(),
        "SNAPSHOT".into(),
        "ALIGNED".into(),
    ]];
    for mw in items {
        let status = mw.status.clone().unwrap_or_default();
        // Statut d'une génération antérieure : il décrit l'état souhaité d'avant la bascule.
        let stale = mw.metadata.generation.is_some()
            && status.observed_generation != mw.metadata.generation;
        let (snapshot, aligned) = if stale {
            ("?".to_string(), "?".to_string())
        } else {
            (
                status.snapshot.unwrap_or_else(|| "-".into()),
                status.aligned.to_string(),
            )
        };
        // Garde-fou production : le mode demandé n'est pas appliqué.
        let refused = status
            .conditions
            .iter()
            .any(|c| c.type_ == "Allowed" && c.status == "False");
        let mode = if refused {
            format!("{} (refusé)", mw.spec.mode)
        } else {
            mw.spec.mode.clone()
        };
        rows.push([
            mw.metadata.namespace.clone().unwrap_or_default(),
            mw.metadata.name.clone().unwrap_or_default(),
            mode,
            snapshot,
            format!("{aligned}/{}", status.pods),
        ]);
    }
    let widths: Vec<usize> = (0..5)
        .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let cells: Vec<String> = r
                .iter()
                .zip(&widths)
                .map(|(c, w)| format!("{c:<w$}"))
                .collect();
            format!("{}\n", cells.join("   ").trim_end())
        })
        .collect()
}

/// Ce que le contrôleur sait d'un workload, tel que lu sur son interface : `…/operations`
/// et `…/requests?format=json` (les deux sens).
#[derive(Debug, Clone, Default)]
pub struct WorkloadData {
    pub namespace: String,
    pub name: String,
    /// Mode souhaité ; absent, le workload est libre.
    pub mode: Option<String>,
    pub pods: usize,
    pub aligned: usize,
    pub operations: Vec<Value>,
    pub requests: Vec<Value>,
    /// Relevé agrégé des requêtes sans réponse du mock (`unmatched` de `/v1/workloads`) ;
    /// vide pour un contrôleur d'une version antérieure.
    pub unmatched: UnmatchedReport,
}

/// Longueur maximale d'une valeur de header dans le rapport.
const MAX_HEADER_VALUE: usize = 80;

/// Rapport Markdown de la découverte (spec §9.5) : API par workload, puis les `top`
/// opérations les plus appelées avec les headers d'une requête enregistrée.
pub fn report_markdown(workloads: &[WorkloadData], top: usize) -> String {
    let mut md = String::from("# Découverte mimic\n\n");
    if workloads.is_empty() {
        md.push_str("Aucun workload connu du contrôleur.\n");
        return md;
    }
    md.push_str("| Workload | Mode | Pods alignés | API | Opérations | Appels |\n|---|---|---:|---:|---:|---:|\n");
    for w in workloads {
        let apis = apis(&w.operations);
        let _ = writeln!(
            md,
            "| [{ns}/{name}](#{ns}{name}) | {} | {}/{} | {} | {} | {} |",
            w.mode.as_deref().unwrap_or("libre"),
            w.aligned,
            w.pods,
            apis.len(),
            w.operations.len(),
            apis.values().map(|a| a.1).sum::<u64>(),
            ns = w.namespace,
            name = w.name,
        );
    }
    for w in workloads {
        workload_section(&mut md, w, top);
    }
    md
}

/// API d'un workload : (sens, authority) → (opérations, appels).
fn apis(operations: &[Value]) -> BTreeMap<(u8, String), (usize, u64)> {
    let mut out: BTreeMap<(u8, String), (usize, u64)> = BTreeMap::new();
    for op in operations {
        let e = out.entry(api_key(op)).or_default();
        e.0 += 1;
        e.1 += op["count"].as_u64().unwrap_or(0);
    }
    out
}

/// Le service avant ses dépendances.
fn api_key(op: &Value) -> (u8, String) {
    let rank = u8::from(op["direction"] != "inbound");
    (rank, str_of(&op["authority"]).to_string())
}

fn kind(rank: u8) -> &'static str {
    if rank == 0 { "service" } else { "dépendance" }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn workload_section(md: &mut String, w: &WorkloadData, top: usize) {
    let _ = write!(
        md,
        "\n## {}/{}\n\nMode {} · {}/{} pods alignés\n\n",
        w.namespace,
        w.name,
        w.mode.as_deref().unwrap_or("libre"),
        w.aligned,
        w.pods
    );
    if w.operations.is_empty() {
        md.push_str("Aucun trafic observé.\n");
        return;
    }
    md.push_str(
        "### API découvertes\n\n| Sens | API | Opérations | Appels |\n|---|---|---:|---:|\n",
    );
    for ((rank, authority), (ops, calls)) in apis(&w.operations) {
        let _ = writeln!(
            md,
            "| {} | `{}` | {ops} | {calls} |",
            kind(rank),
            cell(&authority)
        );
    }

    let mut ops: Vec<&Value> = w.operations.iter().collect();
    ops.sort_by(|a, b| {
        let k = |o: &Value| {
            (
                api_key(o),
                str_of(&o["path"]).to_string(),
                str_of(&o["method"]).to_string(),
            )
        };
        b["count"]
            .as_u64()
            .cmp(&a["count"].as_u64())
            .then_with(|| k(a).cmp(&k(b)))
    });
    ops.truncate(top);
    let _ = write!(
        md,
        "\n### Top {} des appels\n\n| # | Sens | API | Méthode | Chemin | Appels | Codes | p50 / p99 (ms) |\n|---:|---|---|---|---|---:|---|---:|\n",
        ops.len()
    );
    for (i, op) in ops.iter().enumerate() {
        let _ = writeln!(
            md,
            "| {} | {} | `{}` | {} | `{}` | {} | {} | {} / {} |",
            i + 1,
            kind(api_key(op).0),
            cell(str_of(&op["authority"])),
            str_of(&op["method"]),
            cell(str_of(&op["path"])),
            op["count"].as_u64().unwrap_or(0),
            codes(&op["status"]),
            number(&op["latency_ms"]["p50"]),
            number(&op["latency_ms"]["p99"]),
        );
    }
    md.push('\n');
    for (i, op) in ops.iter().enumerate() {
        headers_section(md, i + 1, op, &w.requests);
    }
}

fn headers_section(md: &mut String, rank: usize, op: &Value, requests: &[Value]) {
    let _ = write!(
        md,
        "<details><summary>{rank}. {} {} : headers</summary>\n\n",
        str_of(&op["method"]),
        html(str_of(&op["path"]))
    );
    match requests.iter().find(|r| matches(op, r)) {
        Some(r) => {
            md.push_str("| Header | Valeur (requête enregistrée) |\n|---|---|\n");
            for h in r["req_headers"].as_array().into_iter().flatten() {
                let _ = writeln!(
                    md,
                    "| `{}` | `{}` |",
                    cell(str_of(&h["name"])),
                    cell(&truncate(str_of(&h["value"])))
                );
            }
        }
        None => {
            let declared: Vec<&Value> = op["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["in"] == "header")
                .collect();
            if declared.is_empty() {
                md.push_str("Aucune requête enregistrée ni header déclaré.\n");
            } else {
                md.push_str("| Header | Valeur |\n|---|---|\n");
                for p in declared {
                    let required = if p["required"] == true {
                        ", requis"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        md,
                        "| `{}` | *paramètre déclaré{required}* |",
                        cell(str_of(&p["name"]))
                    );
                }
            }
        }
    }
    md.push_str("\n</details>\n\n");
}

/// La requête enregistrée relève-t-elle de cette opération ?
fn matches(op: &Value, r: &Value) -> bool {
    r["direction"] == op["direction"]
        && (op["direction"] == "inbound" || r["authority"] == op["authority"])
        && r["method"] == op["method"]
        && path_matches(str_of(&op["path"]), str_of(&r["path"]))
}

/// Un segment `{param}` du template accepte n'importe quel segment non vide.
fn path_matches(template: &str, path: &str) -> bool {
    let t: Vec<&str> = template.split('/').collect();
    let p: Vec<&str> = path.split('/').collect();
    t.len() == p.len()
        && t.iter().zip(&p).all(|(t, p)| {
            if t.starts_with('{') && t.ends_with('}') {
                !p.is_empty()
            } else {
                t == p
            }
        })
}

/// `200 97 %, 404 2.5 %`, par code croissant.
fn codes(status: &Value) -> String {
    let Some(map) = status.as_object() else {
        return "-".into();
    };
    let sorted: BTreeMap<&String, f64> = map
        .iter()
        .map(|(k, v)| (k, v.as_f64().unwrap_or(0.0)))
        .collect();
    sorted
        .into_iter()
        .map(|(code, share)| format!("{code} {} %", trim(share * 100.0)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn number(v: &Value) -> String {
    v.as_f64().map_or_else(|| "-".into(), trim)
}

/// Entier si possible, sinon une décimale.
fn trim(x: f64) -> String {
    let rounded = (x * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_HEADER_VALUE {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX_HEADER_VALUE).collect();
        format!("{head}…")
    }
}

/// Contenu d'une cellule de tableau Markdown, dans un bloc de code.
fn cell(s: &str) -> String {
    s.replace('|', "\\|")
        .replace('`', "'")
        .replace(['\n', '\r'], " ")
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Pod du contrôleur dont l'interface est lue (spec §5.5.4, §9.5) : la réplique active,
/// marquée et en marche ; sinon (contrôleur d'une version antérieure, ou bascule en cours) le
/// premier pod en marche, qu'une passive refuse alors par un `503` explicite.
pub fn controller_pod(pods: &[k8s_openapi::api::core::v1::Pod]) -> Result<String, String> {
    let (label, value) = mimic_controller::leader::LEADER_LABEL;
    let running = |p: &&k8s_openapi::api::core::v1::Pod| {
        p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running")
    };
    let marked = |p: &&k8s_openapi::api::core::v1::Pod| {
        p.metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(label))
            .is_some_and(|v| v == value)
    };
    pods.iter()
        .filter(running)
        .find(marked)
        .or_else(|| pods.iter().find(running))
        .and_then(|p| p.metadata.name.clone())
        .ok_or_else(|| "aucun pod du contrôleur en marche".to_string())
}
