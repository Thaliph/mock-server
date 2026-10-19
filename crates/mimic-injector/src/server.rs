//! Adaptateur HTTPS du webhook : `POST /mutate`, `/healthz`, `/metrics`.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use k8s_openapi::api::core::v1::Pod;
use kube::core::DynamicObject;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use mimic_net::serve::{ServeOptions, serve};
use mimic_net::tls::Tls;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use tokio::net::TcpListener;

use crate::config::InjectorConfig;
use crate::inject::{Outcome, mutate, warnings};

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

struct Injector {
    config: InjectorConfig,
    admissions: Family<ResultLabel, Counter<u64, AtomicU64>>,
    registry: Registry,
}

/// Réponse d'admission pour une revue : le pod est toujours admis, avec un patch s'il
/// est injecté, sinon avec un avertissement.
pub fn review(
    review: AdmissionReview<Pod>,
    config: &InjectorConfig,
) -> (&'static str, AdmissionReview<DynamicObject>) {
    let req: AdmissionRequest<Pod> = match review.try_into() {
        Ok(r) => r,
        Err(e) => {
            return (
                "error",
                AdmissionResponse::invalid(format!("revue d'admission invalide: {e}"))
                    .into_review(),
            );
        }
    };
    let resp = AdmissionResponse::from(&req);
    let Some(pod) = req.object.as_ref() else {
        return ("skipped", resp.into_review());
    };
    match mutate(pod, config) {
        Outcome::Injected(new) => {
            let (Ok(before), Ok(after)) = (serde_json::to_value(pod), serde_json::to_value(&*new))
            else {
                return ("error", resp.into_review());
            };
            match resp.clone().with_patch(json_patch::diff(&before, &after)) {
                Ok(mut r) => {
                    let w = warnings(pod);
                    r.warnings = (!w.is_empty()).then_some(w);
                    ("injected", r.into_review())
                }
                Err(e) => {
                    tracing::warn!(error = %e, "patch non sérialisable");
                    ("error", resp.into_review())
                }
            }
        }
        Outcome::Skipped(reason) => {
            let mut resp = resp;
            resp.warnings = Some(vec![format!("mimic non injecté : {reason}")]);
            ("skipped", resp.into_review())
        }
    }
}

/// Sérialise la revue avec le patch en base64, forme documentée de l'API d'admission
/// (`kube` l'écrirait en tableau d'octets).
pub fn to_json(review: &AdmissionReview<DynamicObject>) -> serde_json::Value {
    use base64::Engine;
    let mut v = serde_json::to_value(review).unwrap_or_default();
    if let Some(bytes) = review.response.as_ref().and_then(|r| r.patch.as_ref()) {
        v["response"]["patch"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    v
}

async fn mutate_handler(
    State(inj): State<Arc<Injector>>,
    axum::Json(body): axum::Json<AdmissionReview<Pod>>,
) -> Response {
    let (result, out) = review(body, &inj.config);
    inj.admissions.get_or_create(&ResultLabel { result }).inc();
    tracing::info!(result, "admission");
    axum::Json(to_json(&out)).into_response()
}

async fn metrics(State(inj): State<Arc<Injector>>) -> Response {
    let mut out = String::new();
    match prometheus_client::encoding::text::encode(&mut out, &inj.registry) {
        Ok(()) => ([("content-type", "text/plain; version=0.0.4")], out).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn router(config: InjectorConfig) -> Router {
    let admissions = Family::<ResultLabel, Counter<u64, AtomicU64>>::default();
    let mut registry = Registry::default();
    registry.register(
        "mimic_injector_admissions",
        "Admissions de pods traitées",
        admissions.clone(),
    );
    let inj = Arc::new(Injector {
        config,
        admissions,
        registry,
    });
    Router::new()
        .route("/mutate", post(mutate_handler))
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .with_state(inj)
}

pub struct Running {
    pub addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl Running {
    pub async fn wait(self) {
        let _ = self.task.await;
    }

    /// Arrêt brutal (tests) : les connexions sont coupées.
    pub fn shutdown(self) {
        self.task.abort();
    }

    /// Arrêt propre (`SIGTERM`) : l'écoute cesse, les admissions en cours finissent.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

pub async fn start(config: InjectorConfig) -> Result<Running, String> {
    let tls = config
        .tls
        .as_ref()
        .map(|files| Tls::new(files, &[]))
        .transpose()?;
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|e| format!("écoute sur {}: {e}", config.listen))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let app = router(config);
    tracing::info!(%addr, tls = tls.is_some(), "webhook d'injection démarré");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, app, tls, ServeOptions::default(), async {
        let _ = stopped.await;
    }));
    Ok(Running { addr, task, stop })
}
