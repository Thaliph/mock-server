//! Réconciliation de `MimicWorkload` et alignement des pods (spec §5.5.1) : fonctions pures.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::crd::MimicWorkloadSpec;
use crate::crd::{
    INBOUND_CAPTURES, LabelSelector, MOCK_WEBSOCKETS, MODES, MimicWorkloadStatus, StatusCondition,
};

/// Label de namespace qui interdit le mock (spec §1 et §5.5.1).
pub const PRODUCTION_LABEL: (&str, &str) = ("env", "production");
/// Raison du refus, reprise dans les conditions `Aligned` et `Allowed`.
pub const PRODUCTION_REASON: &str = "ProductionNamespace";
/// Mode imposé à la place d'un mock refusé.
const FALLBACK_MODE: &str = "record";

/// État souhaité transmis aux sidecars ; vide, chaque sidecar garde le mode et la capture
/// de sa configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub mode: Option<String>,
    pub snapshot: Option<String>,
    /// Capture de l'entrant imposée (`full`, `metadata`, `off`, spec §5.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_inbound: Option<String>,
    /// WebSocket sortant en mock imposé (`relay`, `refuse`, spec §3.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock_websocket: Option<String>,
    /// Pods concernés ; jamais transmis aux sidecars, qui reçoivent leur propre état.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<LabelSelector>,
}

/// Vrai si l'état souhaité s'applique à un pod portant ces labels.
pub fn selects(desired: &Desired, labels: &BTreeMap<String, String>) -> bool {
    desired.selector.as_ref().is_none_or(|s| s.matches(labels))
}

/// État souhaité transmis à un pod : celui du workload s'il est sélectionné, sinon un état
/// vide, c'est-à-dire le mode et la capture de sa configuration (spec §5.5.1).
pub fn desired_for_pod(desired: &Desired, labels: &BTreeMap<String, String>) -> Desired {
    if selects(desired, labels) {
        Desired {
            selector: None,
            ..desired.clone()
        }
    } else {
        Desired::default()
    }
}

/// État souhaité de la ressource une fois son snapshot connu.
pub fn desired_of(spec: &MimicWorkloadSpec, snapshot: Option<String>) -> Desired {
    Desired {
        mode: Some(spec.mode.clone()),
        snapshot,
        capture_inbound: spec.capture.as_ref().and_then(|c| c.inbound.clone()),
        mock_websocket: spec.mock.as_ref().and_then(|m| m.websocket.clone()),
        selector: spec.selector.clone(),
    }
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
            ..d
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
            ..desired
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
    let Some(spec) = spec else {
        return Outcome {
            desired: Desired::default(),
            action: Action::None,
            error: None,
        };
    };
    let set = |snapshot: Option<String>| Outcome {
        desired: desired_of(spec, snapshot),
        action: Action::None,
        error: None,
    };
    if !MODES.contains(&spec.mode.as_str()) {
        return keep("InvalidMode");
    }
    let capture = spec.capture.as_ref().and_then(|c| c.inbound.as_deref());
    if capture.is_some_and(|c| !INBOUND_CAPTURES.contains(&c)) {
        return keep("InvalidCapture");
    }
    let websocket = spec.mock.as_ref().and_then(|m| m.websocket.as_deref());
    if websocket.is_some_and(|w| !MOCK_WEBSOCKETS.contains(&w)) {
        return keep("InvalidMock");
    }
    if !is_mock(&spec.mode) {
        return set(None);
    }
    if let Some(pinned) = &spec.snapshot {
        return if exists(pinned) {
            set(Some(pinned.clone()))
        } else {
            keep("SnapshotNotFound")
        };
    }
    // Le snapshot courant est gardé tant que le workload reste en mock.
    match &current.snapshot {
        Some(id) if current.mode.as_deref().is_some_and(is_mock) && exists(id) => {
            set(Some(id.clone()))
        }
        _ => Outcome {
            desired: current.clone(),
            action: Action::BuildSnapshot,
            error: None,
        },
    }
}

/// Ce qu'un sidecar annonce dans son battement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodReport {
    pub mode: String,
    pub snapshot: Option<String>,
    /// Capture de l'entrant appliquée ; absente chez un ancien sidecar.
    #[serde(default)]
    pub capture_inbound: Option<String>,
    /// WebSocket sortant en mock appliqué ; absent chez un ancien sidecar.
    #[serde(default)]
    pub mock_websocket: Option<String>,
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
        Some(m)
            if *m == pod.mode
                && desired.snapshot == pod.snapshot
                && desired
                    .capture_inbound
                    .as_ref()
                    .is_none_or(|c| pod.capture_inbound.as_ref() == Some(c))
                && desired
                    .mock_websocket
                    .as_ref()
                    .is_none_or(|w| pod.mock_websocket.as_ref() == Some(w)) =>
        {
            PodState::Aligned
        }
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
