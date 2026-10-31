//! Admin listener: `/healthz`, `/readyz`, `/metrics` (spec §9), and graceful shutdown on
//! `SIGTERM` (FND-05).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::metrics::Metrics;

type Check = dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync;

/// What `/readyz` asks: `Ok` when the component can serve, `Err(reason)` otherwise.
#[derive(Clone)]
pub struct Readiness(Arc<Check>);

impl Readiness {
    pub fn new<F, Fut>(check: F) -> Readiness
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        Readiness(Arc::new(move || Box::pin(check())))
    }

    pub fn always() -> Readiness {
        Readiness::new(|| async { Ok(()) })
    }
}

#[derive(Clone)]
struct AdminState {
    metrics: Arc<Metrics>,
    readiness: Readiness,
}

pub fn router(metrics: Arc<Metrics>, readiness: Readiness) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .with_state(AdminState { metrics, readiness })
}

async fn readyz(State(state): State<AdminState>) -> impl IntoResponse {
    match (state.readiness.0)().await {
        Ok(()) => (StatusCode::OK, "ready\n".to_string()),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {reason}\n"),
        ),
    }
}

async fn metrics_handler(State(state): State<AdminState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        state.metrics.encode(),
    )
}

/// Resolves on `SIGTERM` or `SIGINT`.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Serves `router` on `listener` until `SIGTERM`, then drains for at most `grace`.
pub async fn serve(listener: TcpListener, router: Router, grace: Duration) -> std::io::Result<()> {
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let mut drain_rx = stop_tx.subscribe();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        let _ = stop_rx.wait_for(|stop| *stop).await;
    });
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!(grace_ms = grace.as_millis() as u64, "shutdown started");
        let _ = stop_tx.send(true);
    });
    let deadline = async move {
        let _ = drain_rx.wait_for(|stop| *stop).await;
        tokio::time::sleep(grace).await;
    };
    tokio::select! {
        result = server => result?,
        () = deadline => tracing::warn!("grace period over, closing remaining connections"),
    }
    tracing::info!("shutdown complete");
    Ok(())
}

/// Binds the admin listener and logs `admin listening` with the bound address.
pub async fn bind(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "admin listening");
    Ok(listener)
}
