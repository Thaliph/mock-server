//! TLS interception support of the sidecar (spec §5, §7.2, §10.3): configuration, the CA client
//! that obtains and renews the per-pod intermediate, leaf issuance, and the `mimic-trust` volume.

pub mod ca;
pub mod intercept;

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::config::{ConfigError, Env};

const EXCLUDE_VAR: &str = "MIMIC_TLS_EXCLUDE";
const CA_VAR: &str = "MIMIC_TLS_CA";
const URL_VAR: &str = "MIMIC_CONTROLLER_URL";
const CONTROLLER_CA_VAR: &str = "MIMIC_CONTROLLER_CA";
const TOKEN_VAR: &str = "MIMIC_TLS_TOKEN_FILE";
const TRUST_DIR_VAR: &str = "MIMIC_TRUST_DIR";
const TRUST_BINARY_VAR: &str = "MIMIC_TRUST_BINARY";
const WORKLOAD_VAR: &str = "MIMIC_WORKLOAD";
const DEFAULT_TOKEN: &str = "/var/run/secrets/mimic.platform.io/token";
const DEFAULT_TRUST_DIR: &str = "/mimic";

/// Where the image ships the static `mimic-trust` binary.
pub const TRUST_BINARY: &str = "/usr/local/bin/mimic-trust";
/// System stores of the sidecar's own image, first existing wins (same list as `mimic-trust`).
pub const SYSTEM_STORES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
];

/// What the sidecar needs to ask the controller for an intermediate.
#[derive(Debug, Clone)]
pub struct CaConfig {
    /// The deployment root certificate: PEM as given, and its DER.
    pub root_pem: String,
    pub root_der: Vec<u8>,
    /// Sidecar API base URL, without trailing slash.
    pub controller_url: String,
    pub controller_ca: String,
    pub token_file: PathBuf,
    /// Deployment name (`MIMIC_WORKLOAD`).
    pub deployment: String,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub exclusions: Exclusions,
    /// `None`: no interception, outbound TLS is passed through (`no_ca`).
    pub ca: Option<CaConfig>,
    pub trust_dir: PathBuf,
    /// The `mimic-trust` binary to publish (`MIMIC_TRUST_BINARY`).
    pub trust_binary: PathBuf,
}

impl TlsConfig {
    pub fn from_env(env: &Env) -> Result<TlsConfig, ConfigError> {
        let exclusions = match env.optional(EXCLUDE_VAR) {
            None => Exclusions::default(),
            Some(value) => Exclusions::parse(value)
                .map_err(|reason| ConfigError::invalid(EXCLUDE_VAR, value, reason))?,
        };
        let trust_dir = PathBuf::from(env.optional(TRUST_DIR_VAR).unwrap_or(DEFAULT_TRUST_DIR));
        let trust_binary = PathBuf::from(env.optional(TRUST_BINARY_VAR).unwrap_or(TRUST_BINARY));
        let ca = match env.optional(CA_VAR) {
            None => None,
            Some(pem) => Some(ca_config(env, pem)?),
        };
        Ok(TlsConfig {
            exclusions,
            ca,
            trust_dir,
            trust_binary,
        })
    }
}

fn ca_config(env: &Env, pem: &str) -> Result<CaConfig, ConfigError> {
    let url = env.required(URL_VAR)?;
    if !url.starts_with("https://") {
        return Err(ConfigError::invalid(
            URL_VAR,
            url,
            "expected an https:// URL",
        ));
    }
    let controller_ca = env.required(CONTROLLER_CA_VAR)?;
    if ca::first_certificate(controller_ca).is_none() {
        return Err(ConfigError::invalid(
            CONTROLLER_CA_VAR,
            "<pem>",
            "not a PEM certificate",
        ));
    }
    let deployment = env.required(WORKLOAD_VAR)?;
    let root_der = ca::first_certificate(pem)
        .ok_or_else(|| ConfigError::invalid(CA_VAR, "<pem>", "not a PEM certificate"))?;
    Ok(CaConfig {
        root_pem: pem.to_string(),
        root_der,
        controller_url: url.trim_end_matches('/').to_string(),
        controller_ca: controller_ca.to_string(),
        token_file: PathBuf::from(env.optional(TOKEN_VAR).unwrap_or(DEFAULT_TOKEN)),
        deployment: deployment.to_string(),
    })
}

/// Copies the `mimic-trust` binary to `dir/bin/mimic-trust` (mode 0755) and the first existing
/// of `stores` to `dir/bin/fallback-ca.pem` (0644), for the `mimic-trust-<n>` init containers
/// (spec §7.1). Both are attempted; the first failure is returned.
pub fn publish_trust(dir: &Path, binary: &Path, stores: &[PathBuf]) -> io::Result<()> {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin)?;
    let copied = copy_atomic(binary, &bin.join("mimic-trust"), 0o755);
    let store = stores
        .iter()
        .find(|s| s.is_file())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no system trust store"))
        .and_then(|s| copy_atomic(s, &bin.join("fallback-ca.pem"), 0o644));
    copied.and(store)
}

/// Copies through a temporary name so that a reader never sees a partial file.
fn copy_atomic(from: &Path, to: &Path, mode: u32) -> io::Result<()> {
    let tmp = to.with_extension("tmp");
    fs::copy(from, &tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, to)
}
