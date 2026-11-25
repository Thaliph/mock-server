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
    let service_type = match env.optional(SERVICE_TYPE).unwrap_or("ClusterIP") {
        "ClusterIP" => ServiceType::ClusterIP,
        "NodePort" => ServiceType::NodePort,
        "LoadBalancer" => ServiceType::LoadBalancer,
        other => {
            return Err(ConfigError::invalid(
                SERVICE_TYPE,
                other,
                "expected ClusterIP, NodePort or LoadBalancer",
            ));
        }
    };
    let max_replicas = number(env, MAX_REPLICAS, 10_u32, 1, 100)?;
    let resources = resources(env.optional(RESOURCES).unwrap_or(DEFAULT_RESOURCES))?;
    let namespaces = match env.optional(NAMESPACES).filter(|v| !v.is_empty()) {
        None => return Ok(None),
        Some(value) => namespaces(value)?,
    };
    if !mesh_enabled {
        return Err(ConfigError::invalid(
            NAMESPACES,
            env.optional(NAMESPACES).unwrap_or_default(),
            "needs MIMIC_MESH=on",
        ));
    }
    Ok(Some(Settings {
        namespaces,
        max_replicas,
        service_type,
        image: image.to_string(),
        log: log.to_string(),
        resources,
    }))
}

/// Comma-separated DNS-1123 labels, at most 256, distinct.
fn namespaces(value: &str) -> Result<BTreeSet<String>, ConfigError> {
    let entries: Vec<&str> = value.split(',').collect();
    if entries.len() > MAX_NAMESPACES {
        return Err(ConfigError::invalid(
            NAMESPACES,
            value,
            format!("at most {MAX_NAMESPACES} namespaces"),
        ));
    }
    let mut set = BTreeSet::new();
    for entry in entries {
        if !cluster_name(entry) {
            return Err(ConfigError::invalid(
                NAMESPACES,
                value,
                format!("{entry:?} is not a DNS-1123 label"),
            ));
        }
        if !set.insert(entry.to_string()) {
            return Err(ConfigError::invalid(
                NAMESPACES,
                value,
                format!("{entry} is listed twice"),
            ));
        }
    }
    Ok(set)
}

/// A JSON `resources` object: `limits` and `requests` only, each a map of quantities.
fn resources(value: &str) -> Result<Value, ConfigError> {
    let invalid = |reason: String| ConfigError::invalid(RESOURCES, value, reason);
    let parsed: Value = serde_json::from_str(value).map_err(|e| invalid(e.to_string()))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| invalid("expected a JSON object".into()))?;
    for (field, entries) in object {
        if field != "limits" && field != "requests" {
            return Err(invalid(format!("unexpected field {field}")));
        }
        let entries = entries
            .as_object()
            .ok_or_else(|| invalid(format!("{field} must be an object")))?;
        for (name, quantity) in entries {
            if !quantity.as_str().is_some_and(is_quantity) {
                return Err(invalid(format!("{field}.{name} is not a quantity")));
            }
        }
    }
    serde_json::from_value::<ResourceRequirements>(parsed.clone())
        .map_err(|e| invalid(e.to_string()))?;
    Ok(parsed)
}

/// A Kubernetes quantity: a decimal number, then a binary (`Ki` to `Ei`) or decimal (`m`, `k` to
/// `E`) suffix or an exponent (`e3`).
fn is_quantity(text: &str) -> bool {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let digits = unsigned
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(unsigned.len());
    let (number, suffix) = unsigned.split_at(digits);
    let number_ok = match number.split_once('.') {
        None => !number.is_empty(),
        Some((whole, fraction)) => {
            !(whole.is_empty() && fraction.is_empty()) && !fraction.contains('.')
        }
    };
    let exponent = |e: &str| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
    };
    number_ok
        && match suffix {
            "" | "m" | "k" | "M" | "G" | "T" | "P" | "E" => true,
            "Ki" | "Mi" | "Gi" | "Ti" | "Pi" | "Ei" => true,
            s => s.strip_prefix(['e', 'E']).is_some_and(exponent),
        }
}
