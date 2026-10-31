//! Process plumbing shared by every mimic binary: configuration from the environment, JSON
//! logs, Prometheus metrics, the admin listener (health, readiness, metrics) and graceful
//! shutdown (spec §9, §10).

pub mod admin;
pub mod config;
pub mod logging;
pub mod metrics;
#[cfg(feature = "testing")]
pub mod testing;

use std::process::ExitCode;

/// Exit code of a process stopped by an invalid configuration (FND-01).
pub const EXIT_CONFIG: u8 = 2;

/// Handles `--version`: prints `<binary> <version>` and tells the caller to exit.
pub fn version_flag(binary: &str, version: &str) -> Option<ExitCode> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{binary} {version}");
        return Some(ExitCode::SUCCESS);
    }
    None
}

/// Reports an invalid configuration on stderr and returns exit code 2. Logging is not set
/// up yet at this point, hence plain stderr.
pub fn config_failure(binary: &str, error: &config::ConfigError) -> ExitCode {
    eprintln!("{binary}: invalid configuration: {error}");
    ExitCode::from(EXIT_CONFIG)
}
