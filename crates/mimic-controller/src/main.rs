//! `mimic-controller`: webhook, reconciler, sidecar API and UI (spec §2, §7). F0 provides the
//! process skeleton and the Valkey connection that gates readiness.

mod cert_manager;
mod certs;
mod loadtest;
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

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::config::{CommonConfig, ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
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
    /// The `MIMIC_LOAD_*` settings; `max_runs` 0 disables load tests (spec §7.8).
    load: loadtest::Config,
}

fn config(env: &Env) -> Result<Config, ConfigError> {
    let webhook = WebhookConfig::from_env(env, VERSION)?;
    let sidecar_api_addr: SocketAddr =
        env.parse_or("MIMIC_SIDECAR_API_ADDR", DEFAULT_SIDECAR_API)?;
    let push = push_config::PushConfig::from_env(env)?;
    let load = loadtest::Config::from_env(
        env,
        &loadtest::Context {
            image: &webhook.inject.sidecar_image,
            controller_url: &webhook.inject.controller_url,
            controller_namespace: &webhook.namespace,
            controller_port: sidecar_api_addr.port(),
            status_interval: push.status_interval,
            pod_window: push.pod_window,
        },
    )?;
    Ok(Config {
        common: CommonConfig::from_env(env, DEFAULT_ADMIN)?,
        valkey: valkey::client_from_env(env)?,
        webhook,
        tls_ca: tls_ca::TlsCaConfig::from_env(env)?,
        sidecar_api_addr,
        push,
        load,
    })
}

/// The `mimic_limit{name}` series (spec §9): the limits of a parsed configuration.
fn limits(config: &Config) -> BTreeMap<&'static str, u64> {
    fn n(value: impl TryInto<u64>) -> u64 {
        value.try_into().unwrap_or(u64::MAX)
    }
    let (push, load) = (&config.push, &config.load);
    BTreeMap::from([
        ("webhook_max_connections", n(config.webhook.max_connections)),
        ("sidecar_api_max_report_bytes", n(push.max_report)),
        ("sidecar_api_body_budget_bytes", n(push.body_budget)),
        ("sidecar_api_max_egress_bytes", n(push.max_egress)),
        ("sidecar_api_egress_budget_bytes", n(push.egress_budget)),
        ("store_max_deployments", n(push.store.max_deployments)),
        ("store_max_endpoints", n(push.store.limits.max_endpoints)),
        (
            "store_max_authorities",
            n(push.store.limits.max_authorities),
        ),
        (
            "store_sample_budget_bytes",
            n(push.store.limits.sample_budget),
        ),
        ("load_max_runs", n(load.max_runs)),
        ("load_max_namespace_runs", n(load.max_namespace_runs)),
        ("load_max_pending", n(load.max_pending)),
        ("load_max_concurrency", n(load.limits.max_concurrency)),
        ("load_max_rate", n(load.limits.max_rate)),
        ("load_max_endpoints", n(load.max_endpoints)),
        ("load_pod_concurrency", n(load.limits.pod_concurrency)),
        ("load_max_plan_bytes", n(load.max_plan)),
        ("load_store_budget_bytes", n(load.store_budget)),
    ])
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
    let ui_metrics = ui::UiMetrics::register(metrics.registry_mut());
    let status_metrics = status::StatusMetrics::register(metrics.registry_mut());
    let load_metrics = loadtest::LoadMetrics::register(metrics.registry_mut());
    // Constant for the process: set once, from the parsed configuration.
    let limit = Family::<Vec<(String, String)>, Gauge>::default();
    metrics
        .registry_mut()
        .register("limit", "Configured limit, by name", limit.clone());
    for (name, value) in limits(&config) {
        limit
            .get_or_create(&vec![("name".to_string(), name.to_string())])
            .set(i64::try_from(value).unwrap_or(i64::MAX));
    }
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
    // The load API (spec §7.8) exists only when load tests are enabled.
    let load = (config.load.max_runs > 0).then(|| sidecar_api::load::LoadApi {
        reviewer: Arc::new(sidecar_api::KubeReviewer::with_audience(
            kube_client.clone(),
            sidecar_api::load::LOAD_AUDIENCE,
        )),
        reviews: sidecar_api::ReviewCache::default(),
        runs: Arc::new(sidecar_api::load::RunIndex::default()),
        slices: Arc::new(sidecar_api::load::SliceCache::default()),
    });
    let slices = load.as_ref().map(|load| load.slices.clone());
    let load_parts = load
        .as_ref()
        .map(|load| (load.runs.clone(), load.slices.clone()));
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
            if let Some(slices) = slices {
                tokio::spawn(sidecar_api::load::watch_slices(client.clone(), slices));
            }
            tokio::spawn(tls_ca::manage(client.clone(), tls_ca, seed, seed_metrics));
            cert_manager::manage(client, certificates, cert, cert_metrics).await;
        }
    });
    let store = store::Store::new(config.valkey.clone(), config.push.store, store_metrics);
    if let Some((runs, slices)) = load_parts {
        let reconciler = Arc::new(loadtest::Reconciler {
            config: config.load.clone(),
            store: store.clone(),
            workloads: cache.clone(),
            tests: Arc::new(loadtest::Tests::default()),
            runs,
            slices,
            cluster: Arc::new(loadtest::KubeCluster::new(kube_client.clone())),
            seed: seed.clone(),
            cert: cert.clone(),
            metrics: load_metrics,
            state: Default::default(),
        });
        let kube_client = kube_client.clone();
        tokio::spawn(async move {
            let client = loop {
                if let Some(client) = kube_client.get() {
                    break client.clone();
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            tokio::spawn(loadtest::watch(client, reconciler.clone()));
            reconciler.run().await;
        });
    }
    let ui_router = ui::router(store.clone(), ui_metrics);
    tokio::spawn(
        status::Reconciler {
            store: store.clone(),
            cache: cache.clone(),
            sink: Arc::new(status::KubeSink::new(kube_client.clone())),
            metrics: status_metrics,
            window: config.push.pod_window,
            apply_timeout: Duration::from_secs(10),
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
        metrics: api_metrics.clone(),
        limiter: sidecar_api::ReviewLimiter::default(),
        rate: sidecar_api::RateLimiter::new(config.push.rate),
        reviews: sidecar_api::ReviewCache::default(),
        budget: sidecar_api::BodyBudget::new(config.push.body_budget),
        store,
        workloads: cache.clone(),
        max_report: config.push.max_report,
        max_push_endpoints: config.push.max_push_endpoints,
        max_egress: config.push.max_egress,
        egress: sidecar_api::EgressCache::new(config.push.egress_budget, &api_metrics),
        egress_logged: sidecar_api::EgressLogged::default(),
        load,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(extra: &[(&str, &str)]) -> Config {
        let required = [
            ("MIMIC_NAMESPACE", "mimic-system"),
            ("MIMIC_WEBHOOK_SERVICE", "mimic"),
            ("MIMIC_WEBHOOK_SECRET", "mimic-webhook-tls"),
            ("MIMIC_WEBHOOK_CONFIGURATION", "mimic-injector"),
            ("MIMIC_INJECT_SIDECAR_IMAGE", "sidecar:1"),
            ("MIMIC_INJECT_INIT_IMAGE", "init:1"),
            ("MIMIC_TLS_CA_SECRET", "mimic-tls-ca"),
            ("MIMIC_VALKEY_URL", "redis://127.0.0.1:6379"),
        ];
        let env = Env::from_pairs(
            required
                .iter()
                .chain(extra)
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        config(&env).expect("valid configuration")
    }

    #[test]
    fn mon_06_the_limits_are_the_spec_names_with_the_parsed_values() {
        let names = [
            "load_max_concurrency",
            "load_max_endpoints",
            "load_max_namespace_runs",
            "load_max_pending",
            "load_max_plan_bytes",
            "load_max_rate",
            "load_max_runs",
            "load_pod_concurrency",
            "load_store_budget_bytes",
            "sidecar_api_body_budget_bytes",
            "sidecar_api_egress_budget_bytes",
            "sidecar_api_max_egress_bytes",
            "sidecar_api_max_report_bytes",
            "store_max_authorities",
            "store_max_deployments",
            "store_max_endpoints",
            "store_sample_budget_bytes",
            "webhook_max_connections",
        ];
        let defaults = limits(&parsed(&[]));
        assert_eq!(defaults.keys().copied().collect::<Vec<_>>(), names);
        assert_eq!(defaults["webhook_max_connections"], 256);
        assert_eq!(defaults["sidecar_api_max_report_bytes"], 32 << 20);
        assert_eq!(defaults["store_max_deployments"], 50);
        assert_eq!(defaults["load_max_runs"], 0);
        assert_eq!(defaults["load_max_plan_bytes"], 8 << 20);
        assert_eq!(defaults["load_max_rate"], 2000);

        let set = limits(&parsed(&[
            ("MIMIC_SIDECAR_API_EGRESS_BUDGET", "16Mi"),
            ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "100"),
            ("MIMIC_LOAD_MAX_RUNS", "4"),
            ("MIMIC_LOAD_MAX_PENDING", "7"),
            ("MIMIC_LOAD_POD_CONCURRENCY", "64"),
        ]));
        assert_eq!(set["sidecar_api_egress_budget_bytes"], 16_777_216);
        assert_eq!(set["webhook_max_connections"], 100);
        assert_eq!(set["load_max_runs"], 4);
        assert_eq!(set["load_max_pending"], 7);
        assert_eq!(set["load_pod_concurrency"], 64);
        assert_eq!(limits(&parsed(&[])), defaults);
    }

    #[test]
    fn mon_06_the_spec_lists_exactly_these_controller_names() {
        let spec = include_str!("../../../docs/SPEC.md");
        let row = spec
            .lines()
            .find(|l| l.trim_start().starts_with("| `mimic_limit`"))
            .expect("the §9 row");
        let controller = row
            .split("controller `")
            .nth(1)
            .and_then(|t| t.split("; sidecar").next())
            .expect("controller names");
        let listed: std::collections::BTreeSet<String> = format!("`{controller}")
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|t| t.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .map(str::to_string)
            .collect();
        let code: std::collections::BTreeSet<String> =
            limits(&parsed(&[])).keys().map(|k| k.to_string()).collect();
        assert_eq!(listed, code);
    }
}
