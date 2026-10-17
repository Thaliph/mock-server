//! Construction des requêtes de `mimicctl` (spec §9.5) : fonctions pures, testées sans
//! cluster. `main.rs` les envoie à l'API Kubernetes ou au contrôleur.

use kube::api::ObjectMeta;
use mimic_controller::crd::{MODES, MimicWorkload, MimicWorkloadSpec};

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
    Ok(MimicWorkload {
        metadata: ObjectMeta {
            name: Some(workload.into()),
            namespace: Some(namespace.into()),
            ..ObjectMeta::default()
        },
        spec: MimicWorkloadSpec {
            mode: mode.into(),
            snapshot: snapshot.map(String::from),
        },
        status: None,
    })
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

pub fn requests_path(namespace: &str, workload: &str, format: &str, limit: usize) -> String {
    format!(
        "{}/requests?direction=inbound&format={format}&limit={limit}",
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
        rows.push([
            mw.metadata.namespace.clone().unwrap_or_default(),
            mw.metadata.name.clone().unwrap_or_default(),
            mw.spec.mode.clone(),
            status.snapshot.unwrap_or_else(|| "-".into()),
            format!("{}/{}", status.aligned, status.pods),
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
