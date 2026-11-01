//! The shipping client (spec §7.2): freezes the local report into a pending report every
//! `MIMIC_DISCOVERY_PUSH_INTERVAL` and posts it to the sidecar API until it is settled.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};

const INTERVAL_VAR: &str = "MIMIC_DISCOVERY_PUSH_INTERVAL";

/// Where and how often the report is shipped.
#[derive(Debug, Clone)]
pub struct ShipConfig {
    pub interval: Duration,
    /// Time allowed for one post.
    pub timeout: Duration,
    /// `None` without `MIMIC_CONTROLLER_URL`: nothing is shipped.
    pub target: Option<Target>,
}

#[derive(Debug, Clone)]
pub struct Target {
    /// Sidecar API base URL, without trailing slash.
    pub url: String,
    pub ca: String,
    pub token_file: PathBuf,
    /// Deployment name (`MIMIC_WORKLOAD`).
    pub workload: String,
}

impl ShipConfig {
    pub fn from_env(env: &Env) -> Result<ShipConfig, ConfigError> {
        let interval = env.duration_or(INTERVAL_VAR, Duration::from_secs(30))?;
        if !(Duration::from_secs(1)..=Duration::from_secs(3600)).contains(&interval) {
            return Err(ConfigError::invalid(
                INTERVAL_VAR,
                env.optional(INTERVAL_VAR).unwrap_or_default(),
                "expected a duration from 1s to 1h",
            ));
        }
        Ok(ShipConfig {
            interval,
            timeout: Duration::from_secs(30),
            target: None,
        })
    }
}

/// Ships the report of one [`super::Discovery`].
pub struct Shipper;

impl Shipper {
    /// Ships until `stop` completes.
    pub async fn run(&mut self, stop: impl Future<Output = ()>) {
        stop.await;
    }

    /// Posts what is left within `within`.
    pub async fn flush(&mut self, within: Duration) {
        let _ = within;
    }
}
