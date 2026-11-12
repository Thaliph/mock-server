//! The mesh variables of the controller (spec §10.2); an invalid value exits 2.

use std::time::Duration;

use mimic_core::mesh::view::Limits;
use mimic_runtime::config::{ConfigError, Env};

use crate::push_config::{GIB, KIB, MIB, number, size, usize_of};

#[derive(Debug)]
pub struct MeshConfig {
    /// `MIMIC_MESH`: `off` serves no view and starts no watch.
    pub enabled: bool,
    /// `MIMIC_CLUSTER_NAME`.
    pub cluster: String,
    /// `MIMIC_MESH_WATCH_TIMEOUT`.
    pub watch_timeout: Duration,
    /// `MIMIC_MESH_DEBOUNCE`.
    pub debounce: Duration,
    /// `MIMIC_MESH_VIEW_CACHE_BYTES`.
    pub cache_bytes: u64,
    /// `MIMIC_SIDECAR_API_MAX_WATCHES`.
    pub max_watches: usize,
    /// `MIMIC_MESH_MAX_SERVICES`, `MIMIC_MESH_MAX_ENDPOINTS` and `MIMIC_MESH_MAX_VIEW_BYTES`.
    pub limits: Limits,
}

impl Default for MeshConfig {
    fn default() -> MeshConfig {
        MeshConfig {
            enabled: true,
            cluster: "local".to_string(),
            watch_timeout: Duration::from_secs(30),
            debounce: Duration::from_millis(100),
            cache_bytes: 64 * MIB,
            max_watches: 10_000,
            limits: Limits::default(),
        }
    }
}

impl MeshConfig {
    pub fn from_env(env: &Env) -> Result<MeshConfig, ConfigError> {
        let defaults = MeshConfig::default();
        let enabled = match env.optional("MIMIC_MESH") {
            None | Some("on") => true,
            Some("off") => false,
            Some(other) => {
                return Err(ConfigError::invalid(
                    "MIMIC_MESH",
                    other,
                    "expected on or off",
                ));
            }
        };
        let cluster = match env.optional("MIMIC_CLUSTER_NAME") {
            None => defaults.cluster,
            Some(name) if cluster_name(name) => name.to_string(),
            Some(name) => {
                return Err(ConfigError::invalid(
                    "MIMIC_CLUSTER_NAME",
                    name,
                    "expected 1 to 63 characters of a-z, 0-9 and -, starting and ending with a letter or digit",
                ));
            }
        };
        let max_view_bytes = size(
            env,
            "MIMIC_MESH_MAX_VIEW_BYTES",
            defaults.limits.max_view_bytes as u64,
            64 * KIB,
            256 * MIB,
        )?;
        Ok(MeshConfig {
            enabled,
            cluster,
            watch_timeout: millis(
                env,
                "MIMIC_MESH_WATCH_TIMEOUT",
                defaults.watch_timeout,
                Duration::from_millis(100),
                Duration::from_secs(600),
            )?,
            debounce: millis(
                env,
                "MIMIC_MESH_DEBOUNCE",
                defaults.debounce,
                Duration::from_millis(10),
                Duration::from_secs(1),
            )?,
            cache_bytes: size(
                env,
                "MIMIC_MESH_VIEW_CACHE_BYTES",
                defaults.cache_bytes,
                max_view_bytes,
                4 * GIB,
            )?,
            max_watches: number(
                env,
                "MIMIC_SIDECAR_API_MAX_WATCHES",
                defaults.max_watches,
                1,
                1_000_000,
            )?,
            limits: Limits {
                max_services: number(
                    env,
                    "MIMIC_MESH_MAX_SERVICES",
                    defaults.limits.max_services,
                    1,
                    100_000,
                )?,
                max_endpoints: number(
                    env,
                    "MIMIC_MESH_MAX_ENDPOINTS",
                    defaults.limits.max_endpoints,
                    1,
                    1_000_000,
                )?,
                max_view_bytes: usize_of(max_view_bytes),
            },
        })
    }
}

fn cluster_name(name: &str) -> bool {
    let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    name.len() <= 63
        && edge(name.chars().next())
        && edge(name.chars().next_back())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A duration of `min..=max`.
fn millis(
    env: &Env,
    var: &str,
    default: Duration,
    min: Duration,
    max: Duration,
) -> Result<Duration, ConfigError> {
    let value = env.duration_or(var, default)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(ConfigError::invalid(
            var,
            &format!("{}ms", value.as_millis()),
            format!(
                "expected from {}ms to {}ms",
                min.as_millis(),
                max.as_millis()
            ),
        ))
    }
}
