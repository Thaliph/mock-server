//! Configuration of the gateway role (spec §10.3).

use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};

/// What `mimic-sidecar` runs as (`MIMIC_ROLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Sidecar,
    Gateway,
}

impl Role {
    pub fn from_env(_env: &Env) -> Result<Role, ConfigError> {
        todo!()
    }
}

pub const MAX_PORTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    pub ports: Vec<u16>,
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_connections: usize,
}

impl GatewayConfig {
    pub fn from_env(_env: &Env) -> Result<GatewayConfig, ConfigError> {
        todo!()
    }
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
        assert_eq!(Role::from_env(&env(&[("MIMIC_ROLE", "")])).unwrap(), Role::Sidecar);
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
            "+80", "080", " 80", "80 ", "80,", ",80", "80,,81", "0", "65536", "99999999999",
            "15100", "80,80", "http", "-1",
        ] {
            let error = ports(bad).unwrap_err();
            assert!(error.contains("MIMIC_GATEWAY_PORTS"), "{bad:?}: {error}");
        }
        let error = ports(&(1..=65).map(|p| p.to_string()).collect::<Vec<_>>().join(","));
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
