//! `mimic-controller`: webhook, reconciler, sidecar API and UI (spec §2, §7). F0 provides the
//! process skeleton and the Valkey connection that gates readiness.

mod cert_manager;
mod certs;
mod push_config;
mod sidecar_api;
mod status;
mod store;
mod tls;
mod tls_ca;
mod ui;
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

const DEFAULT_SIDECAR_API: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8444);

struct Config {
    common: CommonConfig,
    valkey: redis::Client,
    webhook: WebhookConfig,
    tls_ca: tls_ca::TlsCaConfig,
    sidecar_api_addr: SocketAddr,
    push: push_config::PushConfig,
}

fn config(env: &Env) -> Result<Config, ConfigError> {
    Ok(Config {
        common: CommonConfig::from_env(env, DEFAULT_ADMIN)?,
        valkey: valkey::client_from_env(env)?,
        webhook: WebhookConfig::from_env(env, VERSION)?,
        tls_ca: tls_ca::TlsCaConfig::from_env(env)?,
        sidecar_api_addr: env.parse_or("MIMIC_SIDECAR_API_ADDR", DEFAULT_SIDECAR_API)?,
        push: push_config::PushConfig::from_env(env)?,
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
    let api_metrics = sidecar_api::ApiMetrics::register(metrics.registry_mut());
    let seed_metrics = tls_ca::SeedMetrics::register(metrics.registry_mut());
    let store_metrics = store::StoreMetrics::register(metrics.registry_mut());
    let status_metrics = status::StatusMetrics::register(metrics.registry_mut());
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
    // The TLS seed (spec §5) is loaded into this slot by `tls_ca::manage`.
    let seed = Arc::new(tls_ca::SeedSlot::new());
    // The cluster client, for the sidecar API's `TokenReview`s, once it is available.
    let kube_client = Arc::new(tokio::sync::OnceCell::new());
    tokio::spawn({
        let (cache, cert, seed) = (cache.clone(), cert.clone(), seed.clone());
        let kube_client = kube_client.clone();
        let tls_ca = config.tls_ca;
        let certificates = cert_manager::CertConfig {
            namespace: webhook.namespace.clone(),
            service: webhook.service,
            secret: webhook.secret,
            configuration: webhook.configuration,
        };
        async move {
            let client = workloads::client().await;
            let _ = kube_client.set(client.clone());
            tokio::spawn(workloads::watch(client.clone(), cache));
            tokio::spawn(tls_ca::manage(client.clone(), tls_ca, seed, seed_metrics));
            cert_manager::manage(client, certificates, cert, cert_metrics).await;
        }
    });
    let store = store::Store::new(config.valkey.clone(), config.push.store, store_metrics);
    let ui_router = ui::router(store.clone());
    tokio::spawn(
        status::Reconciler {
            store: store.clone(),
            cache: cache.clone(),
            sink: Arc::new(status::KubeSink::new(kube_client.clone())),
            metrics: status_metrics,
            window: config.push.pod_window,
            down: Default::default(),
        }
        .run(config.push.status_interval),
    );
    let health = valkey::Health::spawn(config.valkey);
    let readiness = Readiness::new({
        let (cache, cert, seed) = (cache.clone(), cert.clone(), seed.clone());
        move || {
            let up = health.is_up();
            let (certificate, synced) = (cert.is_loaded(), cache.is_synced());
            let seeded = seed.is_loaded();
            async move {
                let missing: Vec<&str> = [
                    (up, "valkey unreachable"),
                    (certificate, "webhook certificate not loaded"),
                    (synced, "MimicWorkload cache not synced"),
                    (seeded, "TLS seed not loaded"),
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
        cache: cache.clone(),
        seed: seed.clone(),
        cert: cert.clone(),
        roots: webhook::RootCache::default(),
        metrics: admission_metrics,
    });
    let api = Arc::new(sidecar_api::SidecarApi {
        seed: seed.clone(),
        reviewer: Arc::new(sidecar_api::KubeReviewer::new(kube_client)),
        metrics: api_metrics,
        limiter: sidecar_api::ReviewLimiter::default(),
        rate: sidecar_api::RateLimiter::new(config.push.rate),
        reviews: sidecar_api::ReviewCache::default(),
        budget: sidecar_api::BodyBudget::new(config.push.body_budget),
        store,
        workloads: cache.clone(),
        max_report: config.push.max_report,
        max_push_endpoints: config.push.max_push_endpoints,
    });
    let webhook_listener = tokio::net::TcpListener::bind(webhook.addr).await?;
    tracing::info!(addr = %webhook_listener.local_addr()?, "webhook listening");
    let grace = config.common.shutdown_grace;
    let limits = tls::Limits {
        max_connections: webhook.max_connections,
        request_timeout: webhook.request_timeout,
        idle_timeout: tls::IDLE_TIMEOUT,
    };
    let webhook_server = tls::serve(
        webhook_listener,
        cert.clone(),
        webhook::router(admission),
        limits,
        grace,
        shutdown.clone().wait(),
    );
    // Same serving certificate and limits, with a connection budget of its own.
    let api_listener = tokio::net::TcpListener::bind(config.sidecar_api_addr).await?;
    tracing::info!(addr = %api_listener.local_addr()?, "sidecar api listening");
    let api_server = tls::serve(
        api_listener,
        cert,
        sidecar_api::router(api),
        limits,
        grace,
        shutdown.clone().wait(),
    );

    // Plain HTTP, loopback peers only (spec §7.5).
    let ui_listener = tokio::net::TcpListener::bind(config.push.ui_addr).await?;
    tracing::info!(addr = %ui_listener.local_addr()?, "read api listening");
    let ui_server = admin::serve_until(ui_listener, ui_router, grace, shutdown.clone().wait());

    let listener = admin::bind(config.common.admin_addr).await?;
    let admin_server = admin::serve_until(
        listener,
        admin::router(metrics, readiness),
        grace,
        shutdown.wait(),
    );
    tokio::try_join!(webhook_server, api_server, ui_server, admin_server)?;
    Ok(())
}
