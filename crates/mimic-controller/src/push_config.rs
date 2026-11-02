//! Push API, store and status settings of the controller (spec §10.2).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use mimic_core::discovery::report::Limits;
use mimic_runtime::config::{ConfigError, Env};

use crate::store::StoreConfig;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const DAY: u64 = 86_400;
const DEFAULT_UI: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

pub struct PushConfig {
    /// `MIMIC_SIDECAR_API_RATE`: requests per second per source.
    pub rate: u32,
    /// `MIMIC_SIDECAR_API_MAX_REPORT`, in bytes.
    pub max_report: usize,
    /// `MIMIC_SIDECAR_API_BODY_BUDGET`, in bytes.
    pub body_budget: u64,
    /// `MIMIC_STORE_MAX_PUSH_ENDPOINTS`.
    pub max_push_endpoints: usize,
    pub store: StoreConfig,
    /// `MIMIC_STATUS_INTERVAL` (read by the status reconciler).
    pub status_interval: Duration,
    /// `MIMIC_STATUS_POD_WINDOW`.
    pub pod_window: Duration,
    /// `MIMIC_UI_ADDR` (read by the read API).
    pub ui_addr: SocketAddr,
}

impl PushConfig {
    pub fn from_env(env: &Env) -> Result<PushConfig, ConfigError> {
        let max_report = size(env, "MIMIC_SIDECAR_API_MAX_REPORT", 32 * MIB, 64 * KIB, GIB)?;
        let var = "MIMIC_SIDECAR_API_BODY_BUDGET";
        let body_budget = size(env, var, 64 * MIB, max_report, 4 * GIB)?;
        let max_endpoints = number(env, "MIMIC_STORE_MAX_ENDPOINTS", 512, 1, 10_000)?;
        let var = "MIMIC_STORE_MAX_PUSH_ENDPOINTS";
        let max_push_endpoints = number(env, var, 256, 1, max_endpoints)?;
        Ok(PushConfig {
            rate: number(env, "MIMIC_SIDECAR_API_RATE", 10, 1, 10_000)?,
            max_report: usize_of(max_report),
            body_budget,
            max_push_endpoints,
            store: StoreConfig {
                ttl: duration(env, "MIMIC_STORE_TTL", 30 * DAY, 10, 365 * DAY)?,
                limits: Limits {
                    max_literals: number(env, "MIMIC_STORE_MAX_LITERALS", 32, 2, 1000)?,
                    max_endpoints,
                    sample_budget: usize_of(size(
                        env,
                        "MIMIC_STORE_SAMPLE_BUDGET",
                        4 * MIB,
                        MIB,
                        GIB,
                    )?),
                },
                max_deployments: number(env, "MIMIC_STORE_MAX_DEPLOYMENTS", 50, 1, 100_000)?,
            },
            status_interval: duration(env, "MIMIC_STATUS_INTERVAL", 30, 1, 3600)?,
            pod_window: duration(env, "MIMIC_STATUS_POD_WINDOW", 300, 1, DAY)?,
            ui_addr: env.parse_or("MIMIC_UI_ADDR", DEFAULT_UI)?,
        })
    }
}

fn usize_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// An integer of `min..=max`.
fn number<T>(env: &Env, var: &str, default: T, min: T, max: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
    T::Err: std::fmt::Display,
{
    let n = env.parse_or(var, default)?;
    if (min..=max).contains(&n) {
        Ok(n)
    } else {
        Err(ConfigError::invalid(
            var,
            &n.to_string(),
            format!("expected a number from {min} to {max}"),
        ))
    }
}

/// `<n>`, `<n>Ki`, `<n>Mi` or `<n>Gi` bytes, from `min` to `max`.
fn size(env: &Env, var: &str, default: u64, min: u64, max: u64) -> Result<u64, ConfigError> {
    let Some(value) = env.optional(var) else {
        return Ok(default);
    };
    let (digits, unit) = match value {
        v if v.ends_with("Ki") => (&v[..v.len() - 2], KIB),
        v if v.ends_with("Mi") => (&v[..v.len() - 2], MIB),
        v if v.ends_with("Gi") => (&v[..v.len() - 2], GIB),
        v => (v, 1),
    };
    let bytes = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<u64>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit));
    match bytes {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(ConfigError::invalid(
            var,
            value,
            format!(
                "expected a size (<n>, <n>Ki, <n>Mi or <n>Gi) from {} to {}",
                human(min),
                human(max)
            ),
        )),
    }
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b % GIB == 0 => format!("{}Gi", b / GIB),
        b if b % MIB == 0 => format!("{}Mi", b / MIB),
        b if b % KIB == 0 => format!("{}Ki", b / KIB),
        b => b.to_string(),
    }
}

/// A duration from `min` to `max` seconds.
fn duration(
    env: &Env,
    var: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<Duration, ConfigError> {
    let value = env.duration_or(var, Duration::from_secs(default))?;
    if (Duration::from_secs(min)..=Duration::from_secs(max)).contains(&value) {
        Ok(value)
    } else {
        Err(ConfigError::invalid(
            var,
            env.optional(var).unwrap_or_default(),
            format!("expected a duration from {min}s to {max}s"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mimic_runtime::config::Env;

    use super::*;

    fn env(extra: &[(&str, &str)]) -> Env {
        Env::from_pairs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn ctl_14_defaults() {
        let config = PushConfig::from_env(&env(&[])).unwrap();
        assert_eq!(config.rate, 10);
        assert_eq!(config.max_report, 32 << 20);
        assert_eq!(config.body_budget, 64 << 20);
        assert_eq!(config.max_push_endpoints, 256);
        assert_eq!(config.store.ttl, Duration::from_secs(30 * 86_400));
        assert_eq!(config.store.limits.max_endpoints, 512);
        assert_eq!(config.store.limits.max_literals, 32);
        assert_eq!(config.store.limits.sample_budget, 4 << 20);
        assert_eq!(config.store.max_deployments, 50);
        assert_eq!(config.status_interval, Duration::from_secs(30));
        assert_eq!(config.pod_window, Duration::from_secs(300));
        assert_eq!(config.ui_addr, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn ctl_14_valid_values_are_taken() {
        let config = PushConfig::from_env(&env(&[
            ("MIMIC_SIDECAR_API_RATE", "10000"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "64Ki"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "4Gi"),
            ("MIMIC_STORE_TTL", "2h"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "10"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "10"),
            ("MIMIC_STORE_MAX_LITERALS", "1000"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "1Gi"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "100000"),
            ("MIMIC_STATUS_INTERVAL", "1m"),
            ("MIMIC_STATUS_POD_WINDOW", "24h"),
            ("MIMIC_UI_ADDR", "127.0.0.1:9090"),
        ]))
        .unwrap();
        assert_eq!(config.rate, 10_000);
        assert_eq!(config.max_report, 64 << 10);
        assert_eq!(config.body_budget, 4 << 30);
        assert_eq!(config.store.ttl, Duration::from_secs(7200));
        assert_eq!(config.status_interval, Duration::from_secs(60));
        assert_eq!(config.pod_window, Duration::from_secs(86_400));
        assert_eq!(config.ui_addr.port(), 9090);
        let thirty_days = env(&[("MIMIC_STORE_TTL", "30d")]);
        let config = PushConfig::from_env(&thirty_days).unwrap();
        assert_eq!(config.store.ttl, Duration::from_secs(30 * 86_400));
    }

    #[test]
    fn ctl_14_invalid_values_are_refused_and_named() {
        let invalid: &[(&str, &str)] = &[
            ("MIMIC_SIDECAR_API_RATE", "0"),
            ("MIMIC_SIDECAR_API_RATE", "10001"),
            ("MIMIC_SIDECAR_API_RATE", "fast"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "63Ki"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "2Gi"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "32Mb"),
            ("MIMIC_SIDECAR_API_MAX_REPORT", "Mi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "5Gi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "31Mi"),
            ("MIMIC_STORE_TTL", "9s"),
            ("MIMIC_STORE_TTL", "366d"),
            ("MIMIC_STORE_TTL", "30"),
            ("MIMIC_STORE_TTL", "30w"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "0"),
            ("MIMIC_STORE_MAX_ENDPOINTS", "10001"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "0"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "513"),
            ("MIMIC_STORE_MAX_LITERALS", "1"),
            ("MIMIC_STORE_MAX_LITERALS", "1001"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "512Ki"),
            ("MIMIC_STORE_SAMPLE_BUDGET", "2Gi"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "0"),
            ("MIMIC_STORE_MAX_DEPLOYMENTS", "100001"),
            ("MIMIC_STATUS_INTERVAL", "0s"),
            ("MIMIC_STATUS_INTERVAL", "2h"),
            ("MIMIC_STATUS_POD_WINDOW", "0s"),
            ("MIMIC_STATUS_POD_WINDOW", "25h"),
            ("MIMIC_UI_ADDR", "localhost"),
            ("MIMIC_UI_ADDR", ":8080"),
        ];
        for (var, value) in invalid {
            let error = PushConfig::from_env(&env(&[(var, value)]))
                .err()
                .unwrap_or_else(|| panic!("{var}={value} was accepted"));
            assert!(error.to_string().contains(var), "{var}={value}: {error}");
        }
    }

    #[test]
    fn ctl_14_the_budget_covers_a_report_and_a_push_fits_the_store() {
        let error = PushConfig::from_env(&env(&[
            ("MIMIC_SIDECAR_API_MAX_REPORT", "2Mi"),
            ("MIMIC_SIDECAR_API_BODY_BUDGET", "1Mi"),
        ]))
        .err()
        .expect("budget below the largest push");
        assert!(error.to_string().contains("MIMIC_SIDECAR_API_BODY_BUDGET"));
        // The default push limit (256) is above this store limit.
        let error = PushConfig::from_env(&env(&[("MIMIC_STORE_MAX_ENDPOINTS", "100")]))
            .err()
            .expect("push limit above the store limit");
        assert!(error.to_string().contains("MIMIC_STORE_MAX_PUSH_ENDPOINTS"));
        let ok = env(&[
            ("MIMIC_STORE_MAX_ENDPOINTS", "100"),
            ("MIMIC_STORE_MAX_PUSH_ENDPOINTS", "100"),
        ]);
        assert!(PushConfig::from_env(&ok).is_ok());
    }
}
