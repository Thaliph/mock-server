//! `mimic-sidecar`: transparent proxy injected next to the app (spec §2). F0 provides the
//! process skeleton only: configuration, logs, admin listener, graceful shutdown.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::config::{CommonConfig, Env};
use mimic_runtime::metrics::Metrics;

const BINARY: &str = "mimic-sidecar";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_ADMIN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 15100);

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, VERSION) {
        return code;
    }
    let config = match CommonConfig::from_env(&Env::from_os(), DEFAULT_ADMIN) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    mimic_runtime::logging::init(&config.log);
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "sidecar stopped");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(config: CommonConfig) -> std::io::Result<()> {
    tracing::info!(version = VERSION, "sidecar starting");
    let metrics = Arc::new(Metrics::new("sidecar", VERSION));
    let listener = admin::bind(config.admin_addr).await?;
    admin::serve(
        listener,
        admin::router(metrics, Readiness::always()),
        config.shutdown_grace,
    )
    .await
}
