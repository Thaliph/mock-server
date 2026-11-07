//! `mimic-load`: thin adapter over the library (configuration, logs, exit code).

use std::process::ExitCode;

use mimic_load::config::Config;
use mimic_runtime::config::Env;

const BINARY: &str = "mimic-load";

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, mimic_load::VERSION) {
        return code;
    }
    let config = match Config::from_env(&Env::from_os()) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let mut log = mimic_runtime::logging::init(&config.common.log);
    let code = mimic_load::run(config);
    // Flush the queued lines (the shutdown ones included) before exiting.
    log.finish();
    ExitCode::from(code)
}
