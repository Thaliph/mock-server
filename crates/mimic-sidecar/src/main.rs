//! `mimic-sidecar`: transparent proxy injected next to the app (spec §2): process skeleton
//! (configuration, logs, admin listener, graceful shutdown), the opaque TCP tunnel (INJ-07) and
//! the `/app-health` probe forwarder (INJ-18).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::config::{CommonConfig, Env};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::probes::{self, Probes};
use mimic_sidecar::tunnel::{self, Direction, Shared, TunnelConfig, TunnelMetrics};

const BINARY: &str = "mimic-sidecar";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_ADMIN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 15100);

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, VERSION) {
        return code;
    }
    let env = Env::from_os();
    let config = match CommonConfig::from_env(&env, DEFAULT_ADMIN) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let tunnel = match TunnelConfig::from_env(&env) {
        Ok(tunnel) => tunnel,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let probes = match Probes::from_env(&env) {
        Ok(probes) => probes,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    mimic_runtime::logging::init(&config.log);
    match run(config, tunnel, probes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "sidecar stopped");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run(config: CommonConfig, tunnel: TunnelConfig, probes: Probes) -> std::io::Result<()> {
    tracing::info!(version = VERSION, "sidecar starting");
    let mut metrics = Metrics::new("sidecar", VERSION);
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let metrics = Arc::new(metrics);

    // The listeners are bound before the admin listener serves, so `/readyz` answers 200 only
    // once they accept connections.
    let ipv6 = tunnel::pod_has_ipv6();
    let inbound = tunnel::bind_all(Direction::Inbound, &tunnel.inbound, ipv6)?;
    let outbound = tunnel::bind_all(Direction::Outbound, &tunnel.outbound, ipv6)?;
    let bound = inbound.iter().chain(&outbound).map(|(_, a)| *a).collect();
    let shared = Arc::new(Shared::new(
        bound,
        tunnel_metrics,
        Arc::new(tunnel::kernel_original_dst),
    ));
    for (listener, _) in inbound {
        tunnel::spawn_listener(listener, Direction::Inbound, shared.clone());
    }
    for (listener, _) in outbound {
        tunnel::spawn_listener(listener, Direction::Outbound, shared.clone());
    }

    let listener = admin::bind(config.admin_addr).await?;
    admin::serve(
        listener,
        admin::router(metrics, Readiness::always()).merge(probes::router(Arc::new(probes))),
        config.shutdown_grace,
    )
    .await
}
