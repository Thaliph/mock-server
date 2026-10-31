//! Configuration read from `MIMIC_*` environment variables only (spec §10, FND-01).

use std::collections::BTreeMap;
use std::fmt::Display;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

/// An invalid or missing variable. The message always names the variable; it repeats the
/// value only for variables that are not secrets.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is required")]
    Missing { var: String },
    #[error("{var}={value:?}: {reason}")]
    Invalid {
        var: String,
        value: String,
        reason: String,
    },
    #[error("{var}: {reason}")]
    Rejected { var: String, reason: String },
}

impl ConfigError {
    pub fn invalid(var: &str, value: &str, reason: impl Display) -> ConfigError {
        ConfigError::Invalid {
            var: var.into(),
            value: value.into(),
            reason: reason.to_string(),
        }
    }
}

/// A snapshot of the environment. Empty values count as unset.
#[derive(Debug, Clone, Default)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    pub fn from_os() -> Env {
        Env::from_pairs(std::env::vars())
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Env {
        Env {
            vars: pairs.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
        }
    }

    pub fn optional(&self, var: &str) -> Option<&str> {
        self.vars.get(var).map(String::as_str)
    }

    pub fn required(&self, var: &str) -> Result<&str, ConfigError> {
        self.optional(var)
            .ok_or_else(|| ConfigError::Missing { var: var.into() })
    }

    /// Parses `var` with `FromStr`, or returns `default` when it is unset.
    pub fn parse_or<T>(&self, var: &str, default: T) -> Result<T, ConfigError>
    where
        T: FromStr,
        T::Err: Display,
    {
        match self.optional(var) {
            None => Ok(default),
            Some(value) => value
                .parse()
                .map_err(|e| ConfigError::invalid(var, value, e)),
        }
    }

    /// Parses a duration written `<n>s` or `<n>ms`.
    pub fn duration_or(&self, var: &str, default: Duration) -> Result<Duration, ConfigError> {
        match self.optional(var) {
            None => Ok(default),
            Some(value) => parse_duration(value).ok_or_else(|| {
                ConfigError::invalid(var, value, "expected a duration such as 10s or 500ms")
            }),
        }
    }
}

fn parse_duration(value: &str) -> Option<Duration> {
    if let Some(ms) = value.strip_suffix("ms") {
        return ms.parse().ok().map(Duration::from_millis);
    }
    value
        .strip_suffix('s')?
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// Variables every component reads (spec §10.1).
#[derive(Debug, Clone)]
pub struct CommonConfig {
    pub log: String,
    pub admin_addr: SocketAddr,
    pub shutdown_grace: Duration,
}

impl CommonConfig {
    pub fn from_env(env: &Env, default_admin: SocketAddr) -> Result<CommonConfig, ConfigError> {
        let log = env.optional("MIMIC_LOG").unwrap_or("info").to_string();
        crate::logging::filter(&log).map_err(|e| ConfigError::invalid("MIMIC_LOG", &log, e))?;
        Ok(CommonConfig {
            log,
            admin_addr: env.parse_or("MIMIC_ADMIN_ADDR", default_admin)?,
            shutdown_grace: env.duration_or("MIMIC_SHUTDOWN_GRACE", Duration::from_secs(10))?,
        })
    }
}
