//! Réconciliation de `MimicWorkload` et alignement des pods (spec §5.5.1) : fonctions pures.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::crd::MimicWorkloadSpec;
use crate::crd::{MODES, MimicWorkloadStatus, StatusCondition};

/// Label de namespace qui interdit le mock (spec §1 et §5.5.1).
pub const PRODUCTION_LABEL: (&str, &str) = ("env", "production");
/// Raison du refus, reprise dans les conditions `Aligned` et `Allowed`.
pub const PRODUCTION_REASON: &str = "ProductionNamespace";
/// Mode imposé à la place d'un mock refusé.
const FALLBACK_MODE: &str = "record";

/// État souhaité transmis aux sidecars ; vide, chaque sidecar garde le mode de sa configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub mode: Option<String>,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Construire un snapshot du workload, puis le désigner pour `spec.mode`.
    BuildSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub desired: Desired,
    pub action: Action,
    /// Raison d'échec, reprise dans la condition `Aligned`.
    pub error: Option<String>,
}

/// Vrai pour `mock` et `mock-partial`, les modes que le garde-fou refuse en production.
pub fn is_mock(mode: &str) -> bool {
    mode == "mock" || mode == "mock-partial"
}

/// Vrai si les labels d'un namespace le désignent comme namespace de production.
pub fn is_production(labels: &BTreeMap<String, String>) -> bool {
    labels.get(PRODUCTION_LABEL.0).map(String::as_str) == Some(PRODUCTION_LABEL.1)
}

/// Comme `reconcile`, dans un namespace de production ou non : en production, `mock` et
/// `mock-partial` sont refusés, sans snapshot construit ; un état souhaité déjà en mock
/// repasse en `record` (spec §5.5.1).
pub fn reconcile_guarded(
    spec: Option<&MimicWorkloadSpec>,
    current: &Desired,
    exists: &dyn Fn(&str) -> bool,
    production: bool,
) -> Outcome {
    match spec {
        Some(s) if production && is_mock(&s.mode) => Outcome {
            desired: leave_mock(current.clone()),
            action: Action::None,
            error: Some(PRODUCTION_REASON.into()),
        },
        _ => reconcile(spec, current, exists),
    }
}

/// État souhaité sans mock : inchangé s'il n'en était pas un, sinon `record` sans snapshot.
fn leave_mock(d: Desired) -> Desired {
    if d.mode.as_deref().is_some_and(is_mock) {
        Desired {
            mode: Some(FALLBACK_MODE.into()),
            snapshot: None,
        }
    } else {
        d
    }
}

/// Dernier rempart, au battement : un pod d'un namespace de production ne reçoit jamais de
/// mock, et un pod qui en annonce un (configuration, état rechargé du disque) reçoit `record`.
pub fn guard_pod(desired: Desired, production: bool, reported_mode: &str) -> Desired {
    if !production {
        return desired;
    }
    let desired = leave_mock(desired);
    if desired.mode.is_none() && is_mock(reported_mode) {
        return Desired {
            mode: Some(FALLBACK_MODE.into()),
            snapshot: None,
        };
    }
    desired
}

/// Calcule l'état souhaité à partir de la ressource et de l'état courant.
/// `exists` dit si un snapshot est présent sur le support.
pub fn reconcile(
    spec: Option<&MimicWorkloadSpec>,
    current: &Desired,
    exists: &dyn Fn(&str) -> bool,
) -> Outcome {
    let keep = |error: &str| Outcome {
        desired: current.clone(),
        action: Action::None,
        error: Some(error.into()),
    };
    let set = |mode: &str, snapshot: Option<String>| Outcome {
        desired: Desired {
            mode: Some(mode.into()),
            snapshot,
        },
        action: Action::None,
        error: None,
    };
    let Some(spec) = spec else {
        return Outcome {
            desired: Desired::default(),
            action: Action::None,
            error: None,
        };
    };
    if !MODES.contains(&spec.mode.as_str()) {
        return keep("InvalidMode");
    }
    if !is_mock(&spec.mode) {
        return set(&spec.mode, None);
    }
    if let Some(pinned) = &spec.snapshot {
        return if exists(pinned) {
            set(&spec.mode, Some(pinned.clone()))
        } else {
            keep("SnapshotNotFound")
        };
    }
    // Le snapshot courant est gardé tant que le workload reste en mock.
    match &current.snapshot {
        Some(id) if current.mode.as_deref().is_some_and(is_mock) && exists(id) => {
            set(&spec.mode, Some(id.clone()))
        }
        _ => Outcome {
            desired: current.clone(),
            action: Action::BuildSnapshot,
            error: None,
        },
    }
}

/// Ce qu'un sidecar annonce dans son battement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodReport {
    pub mode: String,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PodState {
    Aligned,
    Diverging,
    Absent,
}

impl PodState {
    pub fn as_str(self) -> &'static str {
        match self {
            PodState::Aligned => "aligned",
            PodState::Diverging => "diverging",
            PodState::Absent => "absent",
        }
    }
}

pub fn pod_state(
    pod: &PodReport,
    last_seen_ms: u64,
    now_ms: u64,
    timeout_ms: u64,
    desired: &Desired,
) -> PodState {
    if now_ms.saturating_sub(last_seen_ms) > timeout_ms {
        return PodState::Absent;
    }
    match &desired.mode {
        None => PodState::Aligned,
        Some(m) if *m == pod.mode && desired.snapshot == pod.snapshot => PodState::Aligned,
        Some(_) => PodState::Diverging,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Pods vus récemment.
    pub pods: u32,
    pub aligned: u32,
    /// Condition `Aligned`.
    pub ready: bool,
    pub reason: Option<String>,
}

pub fn summarize(states: &[PodState], error: Option<&str>) -> Summary {
    let pods = states.iter().filter(|s| **s != PodState::Absent).count() as u32;
    let aligned = states.iter().filter(|s| **s == PodState::Aligned).count() as u32;
    let reason = match error {
        Some(e) => Some(e.to_string()),
        None if pods == 0 => Some("NoPods".into()),
        None if aligned < pods => Some("PodsDiverging".into()),
        None => None,
    };
    Summary {
        pods,
        aligned,
        ready: reason.is_none(),
        reason,
    }
}

/// Statut publié sur la ressource : snapshot servi, pods alignés et conditions `Aligned` et
/// `Allowed` (spec §5.5.1).
pub fn status_of(
    desired: &Desired,
    summary: &Summary,
    generation: Option<i64>,
) -> MimicWorkloadStatus {
    let refused = summary.reason.as_deref() == Some(PRODUCTION_REASON);
    let allowed = if refused {
        StatusCondition {
            type_: "Allowed".into(),
            status: "False".into(),
            reason: Some(PRODUCTION_REASON.into()),
            message: Some(format!(
                "mock et mock-partial sont refusés dans un namespace labellisé {}={}",
                PRODUCTION_LABEL.0, PRODUCTION_LABEL.1
            )),
        }
    } else {
        StatusCondition {
            type_: "Allowed".into(),
            status: "True".into(),
            reason: None,
            message: None,
        }
    };
    MimicWorkloadStatus {
        snapshot: desired.snapshot.clone(),
        pods: summary.pods,
        aligned: summary.aligned,
        observed_generation: generation,
        conditions: vec![
            StatusCondition {
                type_: "Aligned".into(),
                status: if summary.ready { "True" } else { "False" }.into(),
                reason: summary.reason.clone(),
                message: None,
            },
            allowed,
        ],
    }
}
