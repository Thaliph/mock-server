//! The gateway role of `mimic-sidecar` (spec §7.10, `MIMIC_ROLE=gateway`): listeners that answer
//! every request `404` `no-route` until routing exists (#457), bounded for untrusted clients. It
//! reads the common variables, `MIMIC_MAX_CONNECTIONS` and the `MIMIC_GATEWAY_*` ones only.

pub mod config;
pub mod serve;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Shutdown};
use mimic_runtime::config::{CommonConfig, Env};
use mimic_runtime::metrics::Metrics;

use crate::listener;
use config::GatewayConfig;
use serve::Gateway;

const BINARY: &str = "mimic-sidecar";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_ADMIN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 15100);

/// The whole process of the gateway role.
pub fn main(env: &Env) -> ExitCode {
    let common = match CommonConfig::from_env(env, DEFAULT_ADMIN) {
        Ok(common) => common,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let config = match GatewayConfig::from_env(env) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let mut log = mimic_runtime::logging::init(&common.log);
    if let Err(e) = crate::hardening::harden() {
        tracing::error!(error = %e, "cannot harden the process");
        log.finish();
        return ExitCode::FAILURE;
    }
    let code = run(common, config);
    // Flush the queued lines (the shutdown ones included) before exiting.
    log.finish();
    code
}

#[tokio::main]
async fn run(common: CommonConfig, config: GatewayConfig) -> ExitCode {
    let ports = config
        .ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(version = VERSION, ports = %ports, "gateway starting");
    // Registered before any listener is announced: an early SIGTERM is not lost.
    let shutdown = match Shutdown::install() {
        Ok(shutdown) => shutdown,
        Err(e) => {
            tracing::error!(error = %e, "gateway stopped");
            return ExitCode::FAILURE;
        }
    };
    listener::raise_fd_limit();
    let gateway = Gateway::new(config);
    let listeners = match gateway.bind(listener::pod_has_ipv6()) {
        Ok(listeners) => listeners,
        Err(e) => {
            tracing::error!(port = e.port, addr = %e.addr, error = %e.source, "gateway listener failed");
            return ExitCode::FAILURE;
        }
    };
    gateway.spawn(listeners);
    let metrics = Arc::new(Metrics::new("gateway", VERSION));
    let admin_listener = match admin::bind(common.admin_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(error = %e, "gateway stopped");
            return ExitCode::FAILURE;
        }
    };
    let grace = common.shutdown_grace;
    // The admin listener keeps answering (`/readyz` 503) while the connections drain, and stops
    // when the drain ends.
    let drained = {
        let (gateway, shutdown) = (gateway.clone(), shutdown.clone());
        async move {
            shutdown.wait().await;
            gateway.shutdown();
            gateway.drain(grace).await;
        }
    };
    // Whatever the admin drain takes, the process ends with the grace.
    let over = async {
        shutdown.clone().wait().await;
        tokio::time::sleep(grace).await;
    };
    let router = admin::router(metrics, gateway.readiness());
    let served = tokio::select! {
        served = admin::serve_until(admin_listener, router, grace, drained) => served,
        () = over => Ok(()),
    };
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "gateway stopped");
            ExitCode::FAILURE
        }
    }
}
