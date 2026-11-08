//! `mimic-load`: the load generator run by the Job of a `MimicLoadTest` (spec §7.8). The pure
//! choices live in `mimic_core::load`; this crate has the sockets, the timers and the process
//! skeleton.

pub mod api;
pub mod config;
pub mod engine;
pub mod http;
pub mod metrics;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mimic_runtime::admin::{self, Readiness, Shutdown};
use mimic_runtime::metrics::Metrics;

use crate::config::Config;
use crate::metrics::LoadMetrics;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs the admin listener and the generator; returns the process exit code.
#[tokio::main]
pub async fn run(config: Config) -> u8 {
    tracing::info!(version = VERSION, "load generator starting");
    // Registered before any listener is announced: an early SIGTERM is not lost.
    let shutdown = match Shutdown::install() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            tracing::error!(error = %error, "cannot install the signal handlers");
            return 1;
        }
    };
    let mut registry = Metrics::new("load", VERSION);
    let load_metrics = LoadMetrics::register(&mut registry);
    let ready = Arc::new(AtomicBool::new(false));
    let readiness = {
        let ready = ready.clone();
        Readiness::new(move || {
            let ready = ready.load(Ordering::SeqCst);
            async move {
                if ready {
                    Ok(())
                } else {
                    Err("the plan is not read yet".to_string())
                }
            }
        })
    };
    let listener = match admin::bind(config.common.admin_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(error = %error, "cannot bind the admin listener");
            return 1;
        }
    };
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    let grace = config.common.shutdown_grace;
    let server = tokio::spawn(admin::serve_until(
        listener,
        admin::router(Arc::new(registry), readiness),
        grace,
        {
            let stop = shutdown.clone();
            async move {
                tokio::select! {
                    () = stop.wait() => {}
                    _ = finished => {}
                }
            }
        },
    ));
    let code = engine::generate(Arc::new(config), load_metrics, ready, shutdown).await;
    let _ = done.send(());
    let _ = server.await;
    code
}
