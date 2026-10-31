//! `mimic-controller`: webhook, reconciler, sidecar API and UI (spec §2, §7). F0 provides the
//! process skeleton and the Valkey connection that gates readiness.

mod cert_manager;
mod certs;
mod tls;
mod tls_ca;
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
    let _log = mimic_runtime::logging::init(&config.common.log);
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
    // Before any listener is announced: both servers must see a SIGTERM sent right after start.
    let shutdown = admin::Shutdown::install()?;
    tracing::info!(version = VERSION, "controller starting");
    let mut metrics = Metrics::new("controller", VERSION);
    let admission_metrics = webhook::AdmissionMetrics::register(metrics.registry_mut());
    let cert_metrics = cert_manager::CertMetrics::register(metrics.registry_mut());
    let metrics = Arc::new(metrics);
    let webhook = config.webhook;
    tracing::info!(
        service = webhook.service,
        secret = webhook.secret,
        configuration = webhook.configuration,
        "webhook configured"
    );
    let cache = Arc::new(workloads::WorkloadCache::new());
    // The serving certificate is loaded into this slot by the certificate manager.
    let cert = Arc::new(tls::CertSlot::new());
    tokio::spawn({
        let (cache, cert) = (cache.clone(), cert.clone());
        let certificates = cert_manager::CertConfig {
            namespace: webhook.namespace.clone(),
            service: webhook.service,
            secret: webhook.secret,
            configuration: webhook.configuration,
        };
        async move {
            let client = workloads::client().await;
            tokio::spawn(workloads::watch(client.clone(), cache));
            cert_manager::manage(client, certificates, cert, cert_metrics).await;
        }
    });
    let health = valkey::Health::spawn(config.valkey);
    let readiness = Readiness::new({
        let (cache, cert) = (cache.clone(), cert.clone());
        move || {
            let up = health.is_up();
            let (certificate, synced) = (cert.is_loaded(), cache.is_synced());
            async move {
                let missing: Vec<&str> = [
                    (up, "valkey unreachable"),
                    (certificate, "webhook certificate not loaded"),
                    (synced, "MimicWorkload cache not synced"),
                ]
                .into_iter()
                .filter(|(ok, _)| !ok)
                .map(|(_, reason)| reason)
                .collect();
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(missing.join(", "))
                }
            }
        }
    });
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
        tls::Limits {
            max_connections: webhook.max_connections,
            request_timeout: webhook.request_timeout,
            idle_timeout: tls::IDLE_TIMEOUT,
        },
        grace,
        shutdown.clone().wait(),
    );

    let listener = admin::bind(config.common.admin_addr).await?;
    let admin_server = admin::serve_until(
        listener,
        admin::router(metrics, readiness),
        grace,
        shutdown.wait(),
    );
    tokio::try_join!(webhook_server, admin_server)?;
    Ok(())
}
