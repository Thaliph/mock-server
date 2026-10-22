//! Ressource `MimicWorkload` (spec §5.5.1).

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MODES: &[&str] = &["passthrough", "record", "mock", "mock-partial"];
/// Captures de l'entrant (spec §5.2).
pub const INBOUND_CAPTURES: &[&str] = &["full", "metadata", "off"];
/// Traitements d'une poignée de main WebSocket sortante en mock (spec §3.4).
pub const MOCK_WEBSOCKETS: &[&str] = &["relay", "refuse"];

#[derive(CustomResource, Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "mimic.platform.io",
    version = "v1alpha1",
    kind = "MimicWorkload",
    namespaced,
    status = "MimicWorkloadStatus",
    shortname = "mw",
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Snapshot","type":"string","jsonPath":".status.snapshot"}"#,
    printcolumn = r#"{"name":"Aligned","type":"integer","jsonPath":".status.aligned"}"#,
    printcolumn = r#"{"name":"Pods","type":"integer","jsonPath":".status.pods"}"#
)]
pub struct MimicWorkloadSpec {
    /// Mode souhaité pour tous les pods du workload.
    #[schemars(schema_with = "mode_schema")]
    pub mode: String,
    /// Snapshot à rejouer ; absent, le contrôleur en construit un à l'entrée en mock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Seuls les pods sélectionnés appliquent l'état souhaité ; les autres gardent le mode
    /// de leur configuration (spec §5.5.1). Absent : tous les pods du workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<LabelSelector>,
    /// Capture imposée aux sidecars ; absente, chacun garde celle de sa configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureSpec>,
    /// Réglages du mock imposés aux sidecars ; absents, chacun garde ceux de sa configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock: Option<MockSpec>,
}

/// Label selector Kubernetes : `matchLabels` et `matchExpressions`, combinés par un ET.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<LabelSelectorRequirement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LabelSelectorRequirement {
    pub key: String,
    /// `In`, `NotIn`, `Exists` ou `DoesNotExist`.
    pub operator: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

impl LabelSelector {
    /// Vrai si ces labels sont sélectionnés. Un sélecteur vide sélectionne tout ; une
    /// expression invalide (opérateur inconnu, valeurs manquantes ou en trop) ne sélectionne
    /// rien.
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.match_labels
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v))
            && self.match_expressions.iter().all(|r| r.matches(labels))
    }
}

impl LabelSelectorRequirement {
    fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        let value = labels.get(&self.key);
        match self.operator.as_str() {
            "In" if !self.values.is_empty() => value.is_some_and(|v| self.values.contains(v)),
            "NotIn" if !self.values.is_empty() => value.is_none_or(|v| !self.values.contains(v)),
            "Exists" if self.values.is_empty() => value.is_some(),
            "DoesNotExist" if self.values.is_empty() => value.is_none(),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureSpec {
    /// Capture de l'entrant : `full`, `metadata` (sans query, headers ni body, rien de
    /// publié) ou `off` (métriques seules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "capture_schema")]
    pub inbound: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MockSpec {
    /// Poignée de main WebSocket sortante en mock : `relay` (relayée vers la dépendance) ou
    /// `refuse` (réponse `503` sans joindre la dépendance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "mock_websocket_schema")]
    pub websocket: Option<String>,
}

fn mock_websocket_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "enum": MOCK_WEBSOCKETS })
}

fn capture_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "enum": INBOUND_CAPTURES })
}

fn mode_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "enum": MODES })
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MimicWorkloadStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub pods: u32,
    pub aligned: u32,
    /// Requêtes sans réponse du mock relevées par les pods sélectionnés (spec §5.3). Toujours
    /// publié, 0 compris : un patch de fusion le remet ainsi à zéro.
    #[serde(default)]
    pub unmatched: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub conditions: Vec<StatusCondition>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Manifeste de la CRD, tel que livré dans `deploy/controller/crd.yaml`.
pub fn crd_yaml() -> String {
    use kube::CustomResourceExt;
    serde_yaml::to_string(&MimicWorkload::crd()).unwrap_or_default()
}
