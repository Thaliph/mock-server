//! `mimic-sidecar`: transparent proxy injected next to the app (spec §2): process skeleton
//! (configuration, logs, admin listener, graceful shutdown), the connection layer (INJ-07, PRX-05) and
//! the `/app-health` probe forwarder (INJ-18).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Readiness, Shutdown};
use mimic_runtime::config::{CommonConfig, Env};
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::discovery::{Discovery, DiscoveryConfig, ShipConfig};
use mimic_sidecar::listener::{self, Direction, Shared, TunnelConfig, TunnelMetrics};
use mimic_sidecar::probes::{self, Probes};
use mimic_sidecar::red::ProxyMetrics;
use mimic_sidecar::tls::ca::{CaClient, CaMetrics, CaState};
use mimic_sidecar::tls::intercept::{Interception, TlsMetrics};
use mimic_sidecar::tls::{self, TlsConfig};

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
    let tls = match TlsConfig::from_env(&env) {
        Ok(tls) => tls,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let discovery = match DiscoveryConfig::from_env(&env) {
        Ok(discovery) => discovery,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let ship = match ShipConfig::from_env(&env) {
        Ok(ship) => ship,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    let pod = env.optional("MIMIC_POD_NAME").map(str::to_string);
    let mut log = mimic_runtime::logging::init(&config.log);
    // Before any key exists: no core dump, not readable through /proc by other processes.
    if let Err(e) = mimic_sidecar::hardening::harden() {
        tracing::error!(error = %e, "cannot harden the process");
        log.finish();
        return ExitCode::FAILURE;
    }
    let code = match run(config, tunnel, probes, tls, (discovery, ship, pod)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "sidecar stopped");
            ExitCode::FAILURE
        }
    };
    // Flush the queued lines (the shutdown ones included) before exiting.
    log.finish();
    code
}

#[tokio::main]
async fn run(
    config: CommonConfig,
    tunnel: TunnelConfig,
    probes: Probes,
    tls: TlsConfig,
    (discovery, ship, pod): (DiscoveryConfig, ShipConfig, Option<String>),
) -> std::io::Result<()> {
    tracing::info!(version = VERSION, "sidecar starting");
    // Registered before any listener is announced: an early SIGTERM is not lost.
    let shutdown = Shutdown::install()?;
    let mut metrics = Metrics::new("sidecar", VERSION);
    let tunnel_metrics = TunnelMetrics::register(&mut metrics);
    let proxy_metrics = ProxyMetrics::register(&mut metrics, tunnel.max_authorities);
    let ca_metrics = CaMetrics::register(&mut metrics);
    let tls_metrics = TlsMetrics::register(&mut metrics);
    let discovery = Discovery::start(&discovery, pod)?;
    discovery.register(&mut metrics);
    let shipper = discovery.shipper(ship).map_err(std::io::Error::other)?;
    let metrics = Arc::new(metrics);
    // Readiness does not wait for the CA: until it holds an intermediate, TLS is passed through.
    let ca = start_ca(&tls, ca_metrics)?;
    let interception = Interception::new(
        tls.exclusions.clone(),
        ca,
        tls::SYSTEM_STORES.iter().map(Into::into).collect(),
        tls.trust_dir.clone(),
        tls_metrics,
    );

    // The listeners are bound before the admin listener serves, so `/readyz` answers 200 only
    // once they accept connections.
    listener::raise_fd_limit();
    let ipv6 = listener::pod_has_ipv6();
    let inbound = listener::bind_all(Direction::Inbound, &tunnel.inbound, ipv6)?;
    let outbound = listener::bind_all(Direction::Outbound, &tunnel.outbound, ipv6)?;
    let bound = inbound.iter().chain(&outbound).map(|(_, a)| *a).collect();
    let shared = Arc::new(
        Shared::new(
            bound,
            tunnel_metrics,
            Arc::new(listener::kernel_original_dst),
        )
        .with_max_connections(tunnel.max_connections)
        .with_proxy_metrics(proxy_metrics)
        .with_discovery(discovery.tap())
        .with_tls(interception),
    );
    // The listeners stop accepting and the relays wind down on the signal.
    tokio::spawn({
        let (shared, stop) = (shared.clone(), shutdown.clone());
        async move {
            stop.wait().await;
            shared.shutdown();
        }
    });
    for (listener, _) in inbound {
        listener::spawn_listener(listener, Direction::Inbound, shared.clone());
    }
    for (listener, _) in outbound {
        listener::spawn_listener(listener, Direction::Outbound, shared.clone());
    }

    let listener = admin::bind(config.admin_addr).await?;
    let grace = config.shutdown_grace;
    // Responses in flight get the grace period too, in parallel with the admin drain; the
    // connections left at its end are closed.
    // Shipping stops at the signal; once the proxy has drained, what was recorded is flushed
    // within what remains of the grace.
    let shipping = shipper.map(|mut shipper| {
        let stop = shutdown.clone();
        tokio::spawn(async move {
            shipper.run(stop.wait()).await;
            shipper
        })
    });
    let drain = async {
        shutdown.clone().wait().await;
        let begun = std::time::Instant::now();
        shared.drain(grace).await;
        let shipper = match shipping {
            Some(task) => task.await.ok(),
            None => None,
        };
        if let Some(mut shipper) = shipper {
            shipper.flush(grace.saturating_sub(begun.elapsed())).await;
        }
    };
    let (served, ()) = tokio::join!(
        admin::serve_until(
            listener,
            admin::router(metrics, Readiness::always())
                .merge(probes::router(Arc::new(probes)))
                .merge(discovery.router()),
            grace,
            shutdown.clone().wait(),
        ),
        drain
    );
    served
}

/// Publishes the `mimic-trust` volume and starts the renewal task. `None` without
/// `MIMIC_TLS_CA`: no interception, every outbound TLS connection is passed through.
fn start_ca(tls: &TlsConfig, metrics: CaMetrics) -> std::io::Result<Option<Arc<CaState>>> {
    let Some(config) = tls.ca.clone() else {
        tracing::info!("MIMIC_TLS_CA is empty: outbound TLS is passed through");
        return Ok(None);
    };
    let stores: Vec<_> = tls::SYSTEM_STORES.iter().map(Into::into).collect();
    if let Err(e) = tls::publish_trust(&tls.trust_dir, &tls.trust_binary, &stores) {
        // The app images' bundles cannot be built: the apps would not trust the mimic root.
        tracing::error!(error = %e, dir = %tls.trust_dir.display(), "trust volume not published");
        return Ok(None);
    }
    let state = Arc::new(CaState::new(metrics).map_err(std::io::Error::other)?);
    let client = CaClient::new(config, state.clone()).map_err(std::io::Error::other)?;
    tokio::spawn(client.run());
    Ok(Some(state))
}
