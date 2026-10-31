//! `mimic-controller`: webhook, reconciler, sidecar API and UI (spec §2, §7). F0 provides the
//! process skeleton and the Valkey connection that gates readiness.

mod tls;
mod valkey;
mod webhook;
mod workloads;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::config::{CommonConfig, ConfigError, Env};
use mimic_runtime::metrics::Metrics;

const BINARY: &str = "mimic-controller";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_ADMIN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8081);

struct Config {
    common: CommonConfig,
    valkey: redis::Client,
}

fn config(env: &Env) -> Result<Config, ConfigError> {
    Ok(Config {
        common: CommonConfig::from_env(env, DEFAULT_ADMIN)?,
        valkey: valkey::client_from_env(env)?,
    })
}

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, VERSION) {
        return code;
    }
    let config = match config(&Env::from_os()) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    mimic_runtime::logging::init(&config.common.log);
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "controller stopped");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(config: Config) -> std::io::Result<()> {
    tracing::info!(version = VERSION, "controller starting");
    let metrics = Arc::new(Metrics::new("controller", VERSION));
    let health = valkey::Health::spawn(config.valkey);
    let readiness = Readiness::new(move || {
        let up = health.is_up();
        async move {
            if up {
                Ok(())
            } else {
                Err("valkey unreachable".to_string())
            }
        }
    });
    let listener = admin::bind(config.common.admin_addr).await?;
    admin::serve(
        listener,
        admin::router(metrics, readiness),
        config.common.shutdown_grace,
    )
    .await
}
