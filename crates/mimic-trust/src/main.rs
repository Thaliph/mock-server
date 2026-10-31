//! `mimic-trust`: init container run inside each app image. Merges the image's trust store with
//! the deployment root into the files the app is pointed at (spec §7.1), then exits.
//! Statically linked (musl) so that it runs in any Linux image.

use std::path::Path;
use std::process::ExitCode;

use mimic_runtime::config::{ConfigError, Env};
use mimic_trust::{Error, build};

const BINARY: &str = "mimic-trust";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, VERSION) {
        return code;
    }
    let env = Env::from_os();
    let log = env.optional("MIMIC_LOG").unwrap_or("info").to_string();
    if let Err(e) = mimic_runtime::logging::filter(&log) {
        return mimic_runtime::config_failure(BINARY, &ConfigError::invalid("MIMIC_LOG", &log, e));
    }
    // Validate the configuration before logging starts, like the other binaries.
    let result = build(Path::new("/"), &env);
    let _log = mimic_runtime::logging::init(&log);
    match result {
        Ok(report) => {
            if report.no_image_store {
                tracing::warn!("no image trust store");
            }
            tracing::info!(
                sources = report.sources.join(","),
                image_certs = report.image_certs,
                bundle_certs = report.bundle_certs,
                skipped = report.skipped,
                "trust bundle written"
            );
            ExitCode::SUCCESS
        }
        Err(Error::Config(e)) => mimic_runtime::config_failure(BINARY, &e),
        Err(Error::Io(e)) => {
            tracing::error!(error = %e, "cannot write the trust bundle");
            ExitCode::FAILURE
        }
    }
}
