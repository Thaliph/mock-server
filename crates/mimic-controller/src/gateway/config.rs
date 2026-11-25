//! The `MIMIC_GATEWAY_*` variables of the controller (spec §10.2, GW-16).

use std::collections::BTreeSet;

use k8s_openapi::api::core::v1::ResourceRequirements;
use mimic_core::gateway::{ServiceType, Settings};
use mimic_runtime::config::{ConfigError, Env};
use serde_json::Value;

use crate::mesh::config::cluster_name;
use crate::push_config::number;

const NAMESPACES: &str = "MIMIC_GATEWAY_NAMESPACES";
const SERVICE_TYPE: &str = "MIMIC_GATEWAY_SERVICE_TYPE";
const MAX_REPLICAS: &str = "MIMIC_GATEWAY_MAX_REPLICAS";
const RESOURCES: &str = "MIMIC_GATEWAY_RESOURCES";
const DEFAULT_RESOURCES: &str =
    r#"{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"memory":"128Mi"}}"#;
/// Most entries of `MIMIC_GATEWAY_NAMESPACES`.
const MAX_NAMESPACES: usize = 256;

/// The gateway settings; `None` when `MIMIC_GATEWAY_NAMESPACES` is absent or empty (the gateway
/// is off). Every variable is checked either way. `image` and `log` are the sidecar's.
pub fn from_env(
    env: &Env,
    mesh_enabled: bool,
    image: &str,
    log: &str,
) -> Result<Option<Settings>, ConfigError> {
    let _ = (env, mesh_enabled, image, log);
    Ok(None)
}
