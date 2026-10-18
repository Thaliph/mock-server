//! Ressource `MimicWorkload` (spec §5.5.1).

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MODES: &[&str] = &["passthrough", "record", "mock", "mock-partial"];
/// Captures de l'entrant (spec §5.2).
pub const INBOUND_CAPTURES: &[&str] = &["full", "metadata", "off"];

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
    /// Capture imposée aux sidecars ; absente, chacun garde celle de sa configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureSpec>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureSpec {
    /// Capture de l'entrant : `full`, `metadata` (sans query, headers ni body, rien de
    /// publié) ou `off` (métriques seules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "capture_schema")]
    pub inbound: Option<String>,
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
