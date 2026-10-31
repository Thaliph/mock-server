//! `mimic-trust`: builds the trust bundle of one app image (spec §7.1, §10.5).

use std::fmt;
use std::path::Path;

use mimic_runtime::config::{ConfigError, Env};

/// What a successful run wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Files the image's own certificates were read from, in order.
    pub sources: Vec<String>,
    /// The image had no trust store: the fallback store stands in for it.
    pub no_image_store: bool,
    /// Certificates in `image-ca.pem` / `ca-bundle.pem` (the latter includes the root).
    pub image_certs: usize,
    pub bundle_certs: usize,
    /// Unparsable certificate blocks skipped in the sources.
    pub skipped: usize,
}

#[derive(Debug)]
pub enum Error {
    /// Invalid variable or root certificate: exit 2.
    Config(ConfigError),
    /// Cannot write the output: exit 1.
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => e.fmt(f),
            Error::Io(e) => f.write_str(e),
        }
    }
}

/// Builds the bundle. Absolute store paths are looked up under `root` (`/` in production).
pub fn build(root: &Path, env: &Env) -> Result<Report, Error> {
    let _ = (root, env);
    unimplemented!()
}
