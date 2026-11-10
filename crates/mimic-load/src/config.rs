//! `mimic-load` variables (spec §10.1, §10.6).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use mimic_core::load::plan::plan_limit;
use mimic_runtime::config::{CommonConfig, ConfigError, Env};

pub const DEFAULT_ADMIN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 15100);
const DEFAULT_TOKEN: &str = "/var/run/secrets/mimic.platform.io/load-token";
const DEFAULT_MAX_PLAN: usize = 8 << 20;
const HOSTNAME_FILE: &str = "/proc/sys/kernel/hostname";

/// `<namespace>/<name>/<uid>` of the `MimicLoadTest` (`MIMIC_LOAD_RUN`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunId {
    pub namespace: String,
    pub name: String,
    pub uid: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub common: CommonConfig,
    pub run: RunId,
    pub deployment: String,
    /// Load API base URL, without trailing slash.
    pub url: String,
    pub ca: String,
    pub token_file: PathBuf,
    pub credentials_dir: Option<PathBuf>,
    pub interval: Duration,
    pub max_plan: usize,
    /// `MIMIC_LOAD_PROBE_PORT`: the targets' admin port (spec §10.6).
    pub probe_port: u16,
    /// The pod's own name (`MIMIC_POD_NAME`, else the host name).
    pub pod: String,
}

/// A DNS-1123 label (a Kubernetes name part): lower case alphanumerics and `-`, at most 63.
fn label(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

/// A DNS-1123 subdomain: dot separated labels, at most 253.
fn subdomain(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(label)
}

fn run_id(value: &str) -> Result<RunId, ConfigError> {
    let invalid = |reason: &str| ConfigError::invalid("MIMIC_LOAD_RUN", value, reason);
    let mut parts = value.split('/');
    let (Some(namespace), Some(name), Some(uid), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("expected <namespace>/<name>/<uid>"));
    };
    if !label(namespace) || !subdomain(name) {
        return Err(invalid("namespace or name is not a DNS-1123 name"));
    }
    if uid.is_empty() || uid.len() > 64 || !uid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err(invalid("the uid is not a UUID"));
    }
    Ok(RunId {
        namespace: namespace.to_string(),
        name: name.to_string(),
        uid: uid.to_string(),
    })
}

impl Config {
    pub fn from_env(env: &Env) -> Result<Config, ConfigError> {
        let common = CommonConfig::from_env(env, DEFAULT_ADMIN)?;
        let run = run_id(env.required("MIMIC_LOAD_RUN")?)?;
        let deployment = env.required("MIMIC_LOAD_DEPLOYMENT")?;
        if !label(deployment) {
            return Err(ConfigError::invalid(
                "MIMIC_LOAD_DEPLOYMENT",
                deployment,
                "expected a DNS-1123 label",
            ));
        }
        let url = env.required("MIMIC_CONTROLLER_URL")?;
        if !url.starts_with("https://") {
            return Err(ConfigError::invalid(
                "MIMIC_CONTROLLER_URL",
                url,
                "expected an https:// URL",
            ));
        }
        let ca = env.required("MIMIC_CONTROLLER_CA")?;
        if !ca.contains("-----BEGIN CERTIFICATE-----")
            || reqwest::Certificate::from_pem(ca.as_bytes()).is_err()
        {
            return Err(ConfigError::invalid(
                "MIMIC_CONTROLLER_CA",
                "<pem>",
                "not a PEM certificate",
            ));
        }
        let interval = env.duration_or("MIMIC_LOAD_REPORT_INTERVAL", Duration::from_secs(5))?;
        if !(Duration::from_secs(1)..=Duration::from_secs(60)).contains(&interval) {
            return Err(ConfigError::invalid(
                "MIMIC_LOAD_REPORT_INTERVAL",
                env.optional("MIMIC_LOAD_REPORT_INTERVAL")
                    .unwrap_or_default(),
                "expected a duration from 1s to 1m",
            ));
        }
        let max_plan = match env.optional("MIMIC_LOAD_MAX_PLAN") {
            None => DEFAULT_MAX_PLAN,
            Some(value) => plan_limit(value)
                .map_err(|e| ConfigError::invalid("MIMIC_LOAD_MAX_PLAN", value, e))?,
        };
        let probe_port = env.parse_or("MIMIC_LOAD_PROBE_PORT", crate::http::PROBE_PORT)?;
        let pod = env
            .optional("MIMIC_POD_NAME")
            .map(str::to_string)
            .or_else(|| {
                let name = std::fs::read_to_string(HOSTNAME_FILE).ok()?;
                Some(name.trim().to_string()).filter(|n| !n.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_string());
        Ok(Config {
            common,
            run,
            deployment: deployment.to_string(),
            url: url.trim_end_matches('/').to_string(),
            ca: ca.to_string(),
            token_file: PathBuf::from(
                env.optional("MIMIC_LOAD_TOKEN_FILE")
                    .unwrap_or(DEFAULT_TOKEN),
            ),
            credentials_dir: env
                .optional("MIMIC_LOAD_CREDENTIALS_DIR")
                .map(PathBuf::from),
            interval,
            max_plan,
            probe_port,
            pod,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_have_three_parts() {
        assert!(run_id("shop/payment-load/3f2b8c1e-0000-4000-8000-000000000001").is_ok());
        assert!(run_id("shop/payment-load").is_err());
        assert!(run_id("shop/payment-load/uid/extra").is_err());
        assert!(run_id("Shop/payment-load/abc").is_err());
        assert!(run_id("shop/payment-load/not-a-uid-zz").is_err());
    }
}
