//! `MIMIC_DISCOVERY_*` variables (spec §10.3). An invalid value is a configuration error
//! (exit 2).

use mimic_core::discovery::report::Limits;
use mimic_runtime::config::{ConfigError, Env};

const MAX_ENDPOINTS_VAR: &str = "MIMIC_DISCOVERY_MAX_ENDPOINTS";
const MAX_LITERALS_VAR: &str = "MIMIC_DISCOVERY_MAX_LITERALS";
const SAMPLE_BUDGET_VAR: &str = "MIMIC_DISCOVERY_SAMPLE_BUDGET";
const CAPTURE_BUDGET_VAR: &str = "MIMIC_DISCOVERY_CAPTURE_BUDGET";
const QUEUE_VAR: &str = "MIMIC_DISCOVERY_QUEUE";
const REDACT_NAMES_VAR: &str = "MIMIC_DISCOVERY_REDACT_NAMES";

const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
const MAX_REDACT_NAMES: usize = 64;
const MAX_REDACT_NAME_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryConfig {
    /// Endpoints kept per direction.
    pub max_endpoints: usize,
    /// Distinct literal segments below a template node before it absorbs them.
    pub max_literals: usize,
    /// Bytes held by samples.
    pub sample_budget: usize,
    /// Bytes captured in flight and queued.
    pub capture_budget: usize,
    /// Exchange records waiting for the recorder.
    pub queue: usize,
    /// Extra names redacted by name.
    pub redact_names: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        DiscoveryConfig {
            max_endpoints: 256,
            max_literals: 32,
            sample_budget: 16 * MIB,
            capture_budget: 8 * MIB,
            queue: 1024,
            redact_names: Vec::new(),
        }
    }
}

impl DiscoveryConfig {
    pub fn from_env(env: &Env) -> Result<DiscoveryConfig, ConfigError> {
        let default = DiscoveryConfig::default();
        Ok(DiscoveryConfig {
            max_endpoints: number(env, MAX_ENDPOINTS_VAR, default.max_endpoints, 1, 10_000)?,
            max_literals: number(env, MAX_LITERALS_VAR, default.max_literals, 2, 1000)?,
            sample_budget: size(env, SAMPLE_BUDGET_VAR, default.sample_budget)?,
            capture_budget: size(env, CAPTURE_BUDGET_VAR, default.capture_budget)?,
            queue: number(env, QUEUE_VAR, default.queue, 1, 65_536)?,
            redact_names: redact_names(env)?,
        })
    }

    /// The bounds the report enforces.
    pub fn limits(&self) -> Limits {
        Limits {
            max_literals: self.max_literals,
            max_endpoints: self.max_endpoints,
            sample_budget: self.sample_budget,
        }
    }
}

fn number(
    env: &Env,
    var: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ConfigError> {
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

/// `<n>`, `<n>Ki` or `<n>Mi`, from 1Mi to 1Gi.
fn size(env: &Env, var: &str, default: usize) -> Result<usize, ConfigError> {
    let Some(value) = env.optional(var) else {
        return Ok(default);
    };
    let (digits, unit) = match value {
        v if v.ends_with("Ki") => (&v[..v.len() - 2], 1024),
        v if v.ends_with("Mi") => (&v[..v.len() - 2], MIB),
        v => (v, 1),
    };
    let bytes = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit));
    match bytes {
        Some(n) if (MIB..=GIB).contains(&n) => Ok(n),
        _ => Err(ConfigError::invalid(
            var,
            value,
            "expected a size such as 16Mi (<n>, <n>Ki or <n>Mi) from 1Mi to 1Gi",
        )),
    }
}

fn redact_names(env: &Env) -> Result<Vec<String>, ConfigError> {
    let Some(value) = env.optional(REDACT_NAMES_VAR) else {
        return Ok(Vec::new());
    };
    let names: Vec<String> = value.split(',').map(|n| n.trim().to_string()).collect();
    if names.len() > MAX_REDACT_NAMES {
        return Err(ConfigError::invalid(
            REDACT_NAMES_VAR,
            value,
            format!("at most {MAX_REDACT_NAMES} names"),
        ));
    }
    let valid = |n: &String| {
        (1..=MAX_REDACT_NAME_LEN).contains(&n.len())
            && n.bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
    };
    if let Some(bad) = names.iter().find(|n| !valid(n)) {
        return Err(ConfigError::invalid(
            REDACT_NAMES_VAR,
            value,
            format!(
                "{bad:?}: a name is 1 to {MAX_REDACT_NAME_LEN} characters of a-z, 0-9, - and _"
            ),
        ));
    }
    Ok(names)
}
