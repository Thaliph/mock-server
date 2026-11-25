//! Configuration of the gateway role (spec §10.3): `MIMIC_ROLE`, read before any sidecar
//! variable, and the `MIMIC_GATEWAY_*` variables with `MIMIC_MAX_CONNECTIONS`.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};

use crate::listener;

const ROLE_VAR: &str = "MIMIC_ROLE";
const PORTS_VAR: &str = "MIMIC_GATEWAY_PORTS";
const HEADER_TIMEOUT_VAR: &str = "MIMIC_GATEWAY_HEADER_TIMEOUT";
const IDLE_TIMEOUT_VAR: &str = "MIMIC_GATEWAY_IDLE_TIMEOUT";
/// The admin listener's port (spec §9): never a gateway listener.
const ADMIN_PORT: u16 = 15100;
const HEADER_TIMEOUT_RANGE: RangeInclusive<Duration> =
    Duration::from_secs(1)..=Duration::from_secs(5 * 60);
const IDLE_TIMEOUT_RANGE: RangeInclusive<Duration> =
    Duration::from_secs(1)..=Duration::from_secs(60 * 60);

/// What `mimic-sidecar` runs as (`MIMIC_ROLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Sidecar,
    Gateway,
}

impl Role {
    /// Unset or `sidecar`: the sidecar; `gateway`: the gateway role; anything else is invalid.
    pub fn from_env(env: &Env) -> Result<Role, ConfigError> {
        match env.optional(ROLE_VAR) {
            None | Some("sidecar") => Ok(Role::Sidecar),
            Some("gateway") => Ok(Role::Gateway),
            Some(value) => Err(ConfigError::invalid(
                ROLE_VAR,
                value,
                "expected sidecar or gateway",
            )),
        }
    }
}

/// Most listener ports of one gateway.
pub const MAX_PORTS: usize = 64;
/// Longest `MIMIC_GATEWAY_PORTS` value: `MAX_PORTS` ports of 5 digits and their commas.
const MAX_PORTS_LEN: usize = MAX_PORTS * 6;

/// The gateway role's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    /// As written: 1 to 64 distinct ports, never the admin port.
    pub ports: Vec<u16>,
    /// Time allowed for a request head, from the accept or from the first byte after an answer.
    pub header_timeout: Duration,
    /// An idle keep-alive connection is closed after this long.
    pub idle_timeout: Duration,
    /// Connections served at once, over all listeners together.
    pub max_connections: usize,
}

impl GatewayConfig {
    pub fn from_env(env: &Env) -> Result<GatewayConfig, ConfigError> {
        Ok(GatewayConfig {
            ports: ports(env.required(PORTS_VAR)?)?,
            header_timeout: timeout(
                env,
                HEADER_TIMEOUT_VAR,
                Duration::from_secs(10),
                HEADER_TIMEOUT_RANGE,
                "expected a duration from 1s to 5m",
            )?,
            idle_timeout: timeout(
                env,
                IDLE_TIMEOUT_VAR,
                Duration::from_secs(60),
                IDLE_TIMEOUT_RANGE,
                "expected a duration from 1s to 1h",
            )?,
            max_connections: listener::max_connections(env)?,
        })
    }
}

fn ports(value: &str) -> Result<Vec<u16>, ConfigError> {
    let invalid = |reason: &str| ConfigError::invalid(PORTS_VAR, value, reason);
    // Bounded before the split: the work does not grow with the value.
    if value.len() > MAX_PORTS_LEN {
        return Err(ConfigError::Rejected {
            var: PORTS_VAR.into(),
            reason: format!("too long: more than {MAX_PORTS_LEN} bytes"),
        });
    }
    let mut ports = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in value.split(',') {
        // Digits only, no sign, no leading zero, at most 5 of them.
        let digits = !entry.is_empty()
            && entry.len() <= 5
            && entry.bytes().all(|b| b.is_ascii_digit())
            && !entry.starts_with('0');
        let port = entry
            .parse::<u16>()
            .ok()
            .filter(|_| digits)
            .ok_or_else(|| invalid("expected comma-separated numbers from 1 to 65535"))?;
        if port == ADMIN_PORT {
            return Err(invalid("15100 is the admin port"));
        }
        if !seen.insert(port) {
            return Err(invalid("a port is listed twice"));
        }
        ports.push(port);
    }
    if ports.len() > MAX_PORTS {
        return Err(invalid("more than 64 ports"));
    }
    Ok(ports)
}

fn timeout(
    env: &Env,
    var: &str,
    default: Duration,
    range: RangeInclusive<Duration>,
    reason: &str,
) -> Result<Duration, ConfigError> {
    let timeout = env.duration_or(var, default)?;
    if !range.contains(&timeout) {
        return Err(ConfigError::invalid(
            var,
            env.optional(var).unwrap_or_default(),
            reason,
        ));
    }
    Ok(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    fn ports(value: &str) -> Result<Vec<u16>, String> {
        GatewayConfig::from_env(&env(&[("MIMIC_GATEWAY_PORTS", value)]))
            .map(|c| c.ports)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn gw_18_role_is_sidecar_or_gateway_exactly() {
        assert_eq!(Role::from_env(&env(&[])).unwrap(), Role::Sidecar);
        // An empty value is unset, as for every variable (spec §10).
        assert_eq!(
            Role::from_env(&env(&[("MIMIC_ROLE", "")])).unwrap(),
            Role::Sidecar
        );
        assert_eq!(
            Role::from_env(&env(&[("MIMIC_ROLE", "sidecar")])).unwrap(),
            Role::Sidecar
        );
        assert_eq!(
            Role::from_env(&env(&[("MIMIC_ROLE", "gateway")])).unwrap(),
            Role::Gateway
        );
        for bad in ["proxy", "Gateway", " gateway", "gateway "] {
            let error = Role::from_env(&env(&[("MIMIC_ROLE", bad)])).unwrap_err();
            assert!(error.to_string().contains("MIMIC_ROLE"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn gw_18_ports_are_digits_without_sign_leading_zero_or_empty_entry() {
        assert_eq!(ports("80"), Ok(vec![80]));
        assert_eq!(ports("8080,80,65535,1"), Ok(vec![8080, 80, 65535, 1]));
        let many: Vec<String> = (1..=64).map(|p| p.to_string()).collect();
        assert_eq!(ports(&many.join(",")).map(|p| p.len()), Ok(64));
        for bad in [
            "+80",
            "080",
            " 80",
            "80 ",
            "80,",
            ",80",
            "80,,81",
            "0",
            "65536",
            "99999999999",
            "15100",
            "80,80",
            "http",
            "-1",
        ] {
            let error = ports(bad).unwrap_err();
            assert!(error.contains("MIMIC_GATEWAY_PORTS"), "{bad:?}: {error}");
        }
        let error = ports(
            &(1..=65)
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        assert!(error.is_err());
    }

    #[test]
    fn gw_18_a_ports_value_over_the_bound_is_refused_before_the_split() {
        // 64 entries of 6 bytes at most: one byte more is refused whatever it holds.
        let long = format!("{}1", "1,".repeat(MAX_PORTS * 3));
        assert!(long.len() > MAX_PORTS * 6);
        let error = ports(&long).unwrap_err();
        assert!(error.contains("MIMIC_GATEWAY_PORTS"), "{error}");
        assert!(error.contains("too long"), "{error}");
    }

    #[test]
    fn gw_18_timeouts_default_and_bounds() {
        let config = GatewayConfig::from_env(&env(&[("MIMIC_GATEWAY_PORTS", "80")])).unwrap();
        assert_eq!(config.header_timeout, Duration::from_secs(10));
        assert_eq!(config.idle_timeout, Duration::from_secs(60));
        assert_eq!(
            config.max_connections,
            crate::listener::DEFAULT_MAX_CONNECTIONS
        );
        let with = |var: &str, value: &str| {
            GatewayConfig::from_env(&env(&[("MIMIC_GATEWAY_PORTS", "80"), (var, value)]))
        };
        let header = "MIMIC_GATEWAY_HEADER_TIMEOUT";
        let idle = "MIMIC_GATEWAY_IDLE_TIMEOUT";
        assert_eq!(
            with(header, "1s").unwrap().header_timeout,
            Duration::from_secs(1)
        );
        assert_eq!(
            with(header, "5m").unwrap().header_timeout,
            Duration::from_secs(300)
        );
        assert_eq!(
            with(idle, "1s").unwrap().idle_timeout,
            Duration::from_secs(1)
        );
        assert_eq!(
            with(idle, "1h").unwrap().idle_timeout,
            Duration::from_secs(3600)
        );
        for (var, value) in [
            (header, "0s"),
            (header, "500ms"),
            (header, "6m"),
            (header, "soon"),
            (idle, "0s"),
            (idle, "999ms"),
            (idle, "2h"),
        ] {
            let error = with(var, value).unwrap_err().to_string();
            assert!(error.contains(var), "{var}={value}: {error}");
        }
        assert_eq!(
            with("MIMIC_MAX_CONNECTIONS", "4").unwrap().max_connections,
            4
        );
        assert!(with("MIMIC_MAX_CONNECTIONS", "0").is_err());
    }
}
