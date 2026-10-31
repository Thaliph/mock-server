//! FND-02 and FND-03: admin endpoints, tested in process.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mimic_runtime::admin::{self, Readiness};
use mimic_runtime::metrics::Metrics;
use tower::ServiceExt;

async fn get(router: axum::Router, path: &str) -> (StatusCode, String) {
    let response = router
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn router(readiness: Readiness) -> axum::Router {
    admin::router(Arc::new(Metrics::new("sidecar", "1.2.3")), readiness)
}

#[tokio::test]
async fn fnd_02_healthz_answers_200() {
    let (status, _) = get(router(Readiness::always()), "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn fnd_02_readyz_follows_the_readiness_check() {
    let ready = Arc::new(AtomicBool::new(false));
    let flag = ready.clone();
    let readiness = Readiness::new(move || {
        let ok = flag.load(Ordering::SeqCst);
        async move {
            if ok {
                Ok(())
            } else {
                Err("valkey unreachable".to_string())
            }
        }
    });
    let app = router(readiness);

    let (status, body) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("valkey unreachable"), "{body}");

    ready.store(true, Ordering::SeqCst);
    let (status, _) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::OK);

    // Unhealthy readiness never makes the liveness probe fail.
    ready.store(false, Ordering::SeqCst);
    let (status, _) = get(app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn fnd_03_metrics_expose_build_info() {
    let (status, body) = get(router(Readiness::always()), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.lines()
            .any(|l| l == r#"mimic_build_info{component="sidecar",version="1.2.3"} 1"#),
        "{body}"
    );
}

#[tokio::test]
async fn fnd_03_metrics_use_the_prometheus_text_content_type() {
    let response = router(Readiness::always())
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let content_type = response.headers()["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("text/plain")
            || content_type.starts_with("application/openmetrics-text"),
        "{content_type}"
    );
}
