//! `mimic-controller`: webhook, reconciler, sidecar API and UI (spec §2, §7). F0 provides the
//! process skeleton and the Valkey connection that gates readiness.

mod tls;
mod valkey;
mod webhook;
mod webhook_config;
mod workloads;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::config::{CommonConfig, ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use webhook_config::WebhookConfig;

const BINARY: &str = "mimic-controller";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_ADMIN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8081);

struct Config {
    common: CommonConfig,
    valkey: redis::Client,
    webhook: WebhookConfig,
}

fn config(env: &Env) -> Result<Config, ConfigError> {
    Ok(Config {
        common: CommonConfig::from_env(env, DEFAULT_ADMIN)?,
        valkey: valkey::client_from_env(env)?,
        webhook: WebhookConfig::from_env(env, VERSION)?,
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
    let mut metrics = Metrics::new("controller", VERSION);
    let admission_metrics = webhook::AdmissionMetrics::register(metrics.registry_mut());
    let metrics = Arc::new(metrics);
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

    let webhook = config.webhook;
    tracing::info!(
        service = webhook.service,
        secret = webhook.secret,
        configuration = webhook.configuration,
        "webhook configured"
    );
    let cache = Arc::new(workloads::WorkloadCache::new());
    tokio::spawn(workloads::watch(cache.clone()));
    // The serving certificate is loaded into this slot by the certificate manager.
    let cert = Arc::new(tls::CertSlot::new());
    let admission = Arc::new(webhook::Admission {
        controller_namespace: webhook.namespace,
        inject: webhook.inject,
        cache,
        metrics: admission_metrics,
    });
    let webhook_listener = tokio::net::TcpListener::bind(webhook.addr).await?;
    tracing::info!(addr = %webhook_listener.local_addr()?, "webhook listening");
    let grace = config.common.shutdown_grace;
    let webhook_server = tls::serve(
        webhook_listener,
        cert,
        webhook::router(admission),
        grace,
        admin::shutdown_signal(),
    );

    let listener = admin::bind(config.common.admin_addr).await?;
    let admin_server = admin::serve(listener, admin::router(metrics, readiness), grace);
    tokio::try_join!(webhook_server, admin_server)?;
    Ok(())
}
