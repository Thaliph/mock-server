//! Mutating webhook (spec §7.1): three path-scoped endpoints answering `AdmissionReview` v1.
//!
//! Nothing of the pod spec is ever logged or put in a metric label: only the namespace, the
//! pod name, the deployment name, the result and the reason (INJ-09).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use mimic_core::inject::{
    Decision, InjectConfig, JsonPatchOp, NamespaceMode, SkipReason, decide, deployment_of, patch,
};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::workloads::WorkloadCache;

/// Largest `AdmissionReview` accepted (the API server's own limit is 3 MiB).
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// Annotation recording why a pod was not injected.
const SKIPPED_ANNOTATION: &str = "mimic.platform.io/skipped";

type Labels = Vec<(String, String)>;

/// `mimic_webhook_admissions_total{result,reason}`.
#[derive(Clone, Default)]
pub struct AdmissionMetrics {
    admissions: Family<Labels, Counter>,
}

impl AdmissionMetrics {
    pub fn register(registry: &mut Registry) -> AdmissionMetrics {
        let metrics = AdmissionMetrics::default();
        registry.register(
            "webhook_admissions",
            "Admission reviews answered by the injection webhook",
            metrics.admissions.clone(),
        );
        metrics
    }
}

/// What the endpoints need.
pub struct Admission {
    pub controller_namespace: String,
    pub inject: InjectConfig,
    pub cache: Arc<WorkloadCache>,
    pub metrics: AdmissionMetrics,
}

impl AdmissionMetrics {
    fn count(&self, result: &str, reason: &str) {
        self.admissions
            .get_or_create(&vec![
                ("result".to_string(), result.to_string()),
                ("reason".to_string(), reason.to_string()),
            ])
            .inc();
    }
}

pub fn router(admission: Arc<Admission>) -> Router {
    let endpoint = |mode: NamespaceMode| {
        post(
            move |State(admission): State<Arc<Admission>>, body: Body| async move {
                admission.handle(mode, body).await
            },
        )
    };
    Router::new()
        .route("/inject/namespace", endpoint(NamespaceMode::Enabled))
        .route("/inject/workload", endpoint(NamespaceMode::Selected))
        .route("/inject/pod", endpoint(NamespaceMode::Absent))
        .with_state(admission)
}

#[derive(Deserialize)]
struct Review {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    request: Option<Request>,
}

#[derive(Deserialize)]
struct Request {
    uid: String,
    #[serde(default)]
    namespace: String,
    object: Option<Value>,
}

/// What was done with an admitted pod, for the response, the log and the counter.
struct Outcome {
    patch: Vec<JsonPatchOp>,
    deployment: String,
    result: &'static str,
    reason: &'static str,
}

/// Why a review could not be answered with a decision.
struct Failure {
    reason: &'static str,
    message: &'static str,
}

impl Admission {
    async fn handle(&self, mode: NamespaceMode, body: Body) -> Response {
        let bytes = match read_body(body).await {
            Ok(bytes) => bytes,
            Err(status) => {
                let reason = if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "too-large"
                } else {
                    "unreadable"
                };
                self.record("", "", "", "error", reason);
                return status.into_response();
            }
        };
        // Parse errors quote the offending value: they are never logged or returned.
        let Some((uid, namespace, pod)) = parse(&bytes) else {
            self.record("", "", "", "error", "malformed");
            return StatusCode::BAD_REQUEST.into_response();
        };
        let name = pod
            .metadata
            .name
            .as_deref()
            .or(pod.metadata.generate_name.as_deref())
            .unwrap_or_default();
        let response = match self.admit(mode, &namespace, &pod) {
            Ok(outcome) => {
                self.record(
                    &namespace,
                    name,
                    &outcome.deployment,
                    outcome.result,
                    outcome.reason,
                );
                allowed(&uid, &outcome.patch)
            }
            Err(failure) => {
                self.record(&namespace, name, "", "error", failure.reason);
                Some(denied(&uid, failure.message))
            }
        };
        match response {
            Some(json) => Json(json).into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    fn admit(&self, mode: NamespaceMode, namespace: &str, pod: &Pod) -> Result<Outcome, Failure> {
        if mode == NamespaceMode::Selected && !self.cache.is_synced() {
            return Err(Failure {
                reason: "cache-not-synced",
                message: "MimicWorkload cache not synced yet",
            });
        }
        let decision = decide(
            pod,
            namespace,
            &self.controller_namespace,
            mode,
            |deployment| self.cache.has(namespace, deployment),
        );
        Ok(match decision {
            Decision::Inject { deployment } => {
                let patch = patch(pod, &deployment, &self.inject).map_err(|_| Failure {
                    reason: "patch",
                    message: "cannot build the injection patch",
                })?;
                Outcome {
                    patch,
                    deployment,
                    result: "injected",
                    reason: "ok",
                }
            }
            Decision::Skip { reason, deployment } => Outcome {
                patch: vec![skip_patch(pod, reason)],
                deployment: deployment.unwrap_or_default(),
                result: "skipped",
                reason: reason.as_str(),
            },
            Decision::Untouched(reason) => Outcome {
                patch: Vec::new(),
                deployment: deployment_of(pod).unwrap_or_default(),
                result: "skipped",
                reason: reason.as_str(),
            },
        })
    }

    fn record(&self, namespace: &str, pod: &str, deployment: &str, result: &str, reason: &str) {
        self.metrics.count(result, reason);
        tracing::info!(
            target: "webhook",
            namespace,
            pod,
            deployment,
            result,
            reason,
            "admission"
        );
    }
}

/// Reads the body, refusing more than [`MAX_BODY`] bytes.
async fn read_body(body: Body) -> Result<Vec<u8>, StatusCode> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// `(uid, namespace, pod)` of an `AdmissionReview` v1, `None` when malformed.
fn parse(bytes: &[u8]) -> Option<(String, String, Pod)> {
    let review: Review = serde_json::from_slice(bytes).ok()?;
    if review.api_version != "admission.k8s.io/v1" || review.kind != "AdmissionReview" {
        return None;
    }
    let request = review.request?;
    let pod = serde_json::from_value(request.object?).ok()?;
    Some((request.uid, request.namespace, pod))
}

/// The pod is admitted with `patch` (nothing when empty).
fn allowed(uid: &str, patch: &[JsonPatchOp]) -> Option<Value> {
    let mut response = json!({"uid": uid, "allowed": true});
    if !patch.is_empty() {
        let raw = serde_json::to_vec(patch).ok()?;
        response["patchType"] = json!("JSONPatch");
        response["patch"] = json!(base64::engine::general_purpose::STANDARD.encode(raw));
    }
    Some(review_of(response))
}

fn denied(uid: &str, message: &str) -> Value {
    review_of(json!({
        "uid": uid,
        "allowed": false,
        "status": {"code": 500, "message": message},
    }))
}

fn review_of(response: Value) -> Value {
    json!({"apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview", "response": response})
}

/// The `skipped` annotation, the only change made to a skipped pod.
fn skip_patch(pod: &Pod, reason: SkipReason) -> JsonPatchOp {
    if pod.metadata.annotations.is_some() {
        JsonPatchOp {
            op: "add",
            path: format!(
                "/metadata/annotations/{}",
                SKIPPED_ANNOTATION.replace('/', "~1")
            ),
            value: json!(reason.as_str()),
        }
    } else {
        JsonPatchOp {
            op: "add",
            path: "/metadata/annotations".into(),
            value: json!({ SKIPPED_ANNOTATION: reason.as_str() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use base64::Engine;
    use http_body_util::BodyExt;
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use mimic_runtime::metrics::Metrics;
    use serde_json::{Value, json};
    use std::io::Write;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const CANARY: &str = "canary-s3cret-7f3a";

    fn setup(synced: bool, workloads: &[(&str, &str, &str)]) -> (Router, Arc<Metrics>) {
        let mut metrics = Metrics::new("controller", "test");
        let admission = Arc::new(Admission {
            controller_namespace: "mimic-system".into(),
            inject: InjectConfig {
                version: "0.1.0".into(),
                sidecar_image: "mimic-sidecar:test".into(),
                init_image: "mimic-init:test".into(),
                sidecar_resources: ResourceRequirements::default(),
                sidecar_log: "info".into(),
            },
            cache: Arc::new(if synced {
                WorkloadCache::synced_with(workloads)
            } else {
                WorkloadCache::new()
            }),
            metrics: AdmissionMetrics::register(metrics.registry_mut()),
        });
        (router(admission), Arc::new(metrics))
    }

    fn pod() -> Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "generateName": "shop-5d9c-",
                "labels": {"pod-template-hash": "5d9c"},
                "ownerReferences": [{
                    "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "shop-5d9c",
                    "uid": "u", "controller": true,
                }],
            },
            "spec": {"containers": [{
                "name": "app", "image": "app:1",
                "env": [{"name": "TOKEN", "value": CANARY}],
            }]},
        })
    }

    fn review(uid: &str, namespace: &str, pod: &Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
            "request": {"uid": uid, "namespace": namespace, "operation": "CREATE", "object": pod},
        }))
        .unwrap()
    }

    async fn call(router: &Router, method: Method, path: &str, body: Vec<u8>) -> (u16, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn admit(router: &Router, path: &str, body: Vec<u8>) -> Value {
        let (status, bytes) = call(router, Method::POST, path, body).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap()
    }

    fn patch_of(response: &Value) -> Vec<Value> {
        let encoded = response["response"]["patch"].as_str().expect("patch");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    #[tokio::test]
    async fn inj_08_three_paths_answer_with_the_uid_and_a_base64_json_patch() {
        let (router, _) = setup(true, &[("shop", "w", "shop")]);
        for (path, namespace) in [
            ("/inject/namespace", "shop"),
            ("/inject/workload", "shop"),
            ("/inject/pod", "shop"),
        ] {
            let mut object = pod();
            if path == "/inject/pod" {
                object["metadata"]["labels"][mimic_core::inject::INJECT_LABEL] = json!("enabled");
            }
            let out = admit(&router, path, review("uid-1", namespace, &object)).await;
            assert_eq!(out["apiVersion"], "admission.k8s.io/v1", "{path}");
            assert_eq!(out["kind"], "AdmissionReview", "{path}");
            assert_eq!(out["response"]["uid"], "uid-1", "{path}");
            assert_eq!(out["response"]["allowed"], true, "{path}");
            assert_eq!(out["response"]["patchType"], "JSONPatch", "{path}");
            let patch = patch_of(&out);
            assert!(
                patch.iter().any(|op| op["path"] == "/spec/initContainers"),
                "{path}: {patch:?}"
            );
        }
    }

    #[tokio::test]
    async fn inj_08_unselected_pod_is_allowed_without_a_patch() {
        let (router, _) = setup(true, &[]);
        let out = admit(&router, "/inject/workload", review("u2", "shop", &pod())).await;
        assert_eq!(out["response"]["uid"], "u2");
        assert_eq!(out["response"]["allowed"], true);
        assert!(out["response"].get("patch").is_none());
        assert!(out["response"].get("patchType").is_none());
    }

    #[tokio::test]
    async fn inj_08_skipped_pod_only_gets_the_skipped_annotation() {
        let (router, _) = setup(true, &[]);
        let mut object = pod();
        object["spec"]["hostNetwork"] = json!(true);
        let out = admit(&router, "/inject/namespace", review("u3", "shop", &object)).await;
        assert_eq!(
            patch_of(&out),
            vec![json!({
                "op": "add", "path": "/metadata/annotations",
                "value": {"mimic.platform.io/skipped": "host-network"},
            })]
        );
        object["metadata"]["annotations"] = json!({"a": "b"});
        let out = admit(&router, "/inject/namespace", review("u4", "shop", &object)).await;
        assert_eq!(
            patch_of(&out),
            vec![json!({
                "op": "add", "path": "/metadata/annotations/mimic.platform.io~1skipped",
                "value": "host-network",
            })]
        );
    }

    #[tokio::test]
    async fn inj_08_malformed_bodies_get_400() {
        let (router, _) = setup(true, &[]);
        let bad_kind = serde_json::to_vec(&json!({
            "apiVersion": "admission.k8s.io/v1", "kind": "Pod", "request": {"uid": "x"},
        }))
        .unwrap();
        let no_request = serde_json::to_vec(&json!({
            "apiVersion": "admission.k8s.io/v1", "kind": "AdmissionReview",
        }))
        .unwrap();
        let old_version = serde_json::to_vec(&json!({
            "apiVersion": "admission.k8s.io/v1beta1", "kind": "AdmissionReview",
            "request": {"uid": "x", "namespace": "shop", "object": pod()},
        }))
        .unwrap();
        for body in [
            b"not json".to_vec(),
            Vec::new(),
            bad_kind,
            no_request,
            old_version,
        ] {
            let (status, _) = call(&router, Method::POST, "/inject/pod", body).await;
            assert_eq!(status, 400);
        }
    }

    #[tokio::test]
    async fn inj_08_body_over_4_mib_gets_413() {
        let (router, _) = setup(true, &[]);
        let (status, _) = call(
            &router,
            Method::POST,
            "/inject/pod",
            vec![b' '; MAX_BODY + 1],
        )
        .await;
        assert_eq!(status, 413);
        let (status, _) = call(&router, Method::POST, "/inject/pod", vec![b' '; MAX_BODY]).await;
        assert_eq!(
            status, 400,
            "exactly the limit is read, then rejected as malformed"
        );
    }

    #[tokio::test]
    async fn inj_08_other_methods_get_405_and_unknown_paths_404() {
        let (router, _) = setup(true, &[]);
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            for path in ["/inject/namespace", "/inject/pod", "/inject/workload"] {
                let (status, _) = call(&router, method.clone(), path, Vec::new()).await;
                assert_eq!(status, 405, "{method} {path}");
            }
        }
        let (status, _) = call(&router, Method::POST, "/inject", Vec::new()).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn inj_08_internal_error_is_denied_with_a_message() {
        // The workload endpoint cannot tell whether a MimicWorkload exists before the cache
        // is synced: fail closed.
        let (router, _) = setup(false, &[]);
        let out = admit(&router, "/inject/workload", review("u5", "shop", &pod())).await;
        assert_eq!(out["response"]["uid"], "u5");
        assert_eq!(out["response"]["allowed"], false);
        assert!(
            out["response"]["status"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        assert!(out["response"].get("patch").is_none());
        // The other endpoints do not read the cache.
        let out = admit(&router, "/inject/namespace", review("u6", "shop", &pod())).await;
        assert_eq!(out["response"]["allowed"], true);
    }

    #[tokio::test]
    async fn inj_08_answers_only_https() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::TlsConnector;
        use tokio_rustls::rustls::pki_types::ServerName;
        use tokio_rustls::rustls::{ClientConfig, RootCertStore};

        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let slot = Arc::new(crate::tls::CertSlot::new());
        assert!(!slot.is_loaded());
        slot.set_pem(
            cert.pem().as_bytes(),
            signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        assert!(slot.is_loaded());

        let (router, _) = setup(true, &[]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(crate::tls::serve(
            listener,
            slot,
            router,
            std::time::Duration::from_secs(2),
            async move {
                let _ = stop_rx.await;
            },
        ));

        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client));

        let body = review("tls-1", "shop", &pod());
        let request = format!(
            "POST /inject/namespace HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        tls.write_all(request.as_bytes()).await.unwrap();
        tls.write_all(&body).await.unwrap();
        let mut response = String::new();
        let _ = tls.read_to_string(&mut response).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"uid\":\"tls-1\""), "{response}");

        // Plain HTTP gets no HTTP answer.
        let mut plain = tokio::net::TcpStream::connect(addr).await.unwrap();
        plain
            .write_all(b"GET /inject/pod HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut answer = Vec::new();
        let _ = plain.read_to_end(&mut answer).await;
        assert!(!answer.starts_with(b"HTTP/"), "{answer:?}");

        stop_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server stops")
            .unwrap()
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    thread_local! {
        static CURRENT: std::cell::RefCell<Option<Capture>> = const { std::cell::RefCell::new(None) };
    }

    /// Writes to the capture of the current thread's test, if any.
    struct Routed;

    impl Write for Routed {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            CURRENT.with(|c| match c.borrow_mut().as_mut() {
                Some(capture) => capture.write(buf),
                None => Ok(buf.len()),
            })
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Clears the current thread's capture when the test ends.
    struct CaptureGuard;

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            CURRENT.with(|c| c.borrow_mut().take());
        }
    }

    /// One global subscriber for the test binary: a thread-local `set_default` races with the
    /// callsite interest cache when other tests log from threads without a subscriber.
    fn capture_logs() -> (Capture, CaptureGuard) {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_writer(|| Routed)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber");
        });
        let capture = Capture::default();
        CURRENT.with(|c| *c.borrow_mut() = Some(capture.clone()));
        (capture, CaptureGuard)
    }

    fn webhook_logs(capture: &Capture) -> Vec<Value> {
        String::from_utf8(capture.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["target"] == "webhook")
            .collect()
    }

    #[tokio::test]
    async fn inj_09_one_log_line_and_one_counter_per_admission() {
        let (capture, _guard) = capture_logs();
        let (router, metrics) = setup(true, &[]);
        let mut object = pod();
        object["metadata"]["annotations"] = json!({"note": CANARY});
        // injected
        admit(&router, "/inject/namespace", review("a", "shop", &object)).await;
        // skipped by decision
        let mut host = object.clone();
        host["spec"]["hostNetwork"] = json!(true);
        admit(&router, "/inject/namespace", review("b", "shop", &host)).await;
        // untouched
        admit(&router, "/inject/workload", review("c", "shop", &object)).await;
        // error
        let (router_unsynced, _) = setup(false, &[]);
        admit(
            &router_unsynced,
            "/inject/workload",
            review("d", "shop", &object),
        )
        .await;

        let logs = webhook_logs(&capture);
        assert_eq!(logs.len(), 4, "{logs:?}");
        let fields = |v: &Value| {
            (
                v["namespace"].as_str().unwrap_or_default().to_string(),
                v["pod"].as_str().unwrap_or_default().to_string(),
                v["deployment"].as_str().unwrap_or_default().to_string(),
                v["result"].as_str().unwrap_or_default().to_string(),
                v["reason"].as_str().unwrap_or_default().to_string(),
            )
        };
        let s = |a: &str, b: &str, c: &str, d: &str, e: &str| {
            (
                a.to_string(),
                b.to_string(),
                c.to_string(),
                d.to_string(),
                e.to_string(),
            )
        };
        assert_eq!(
            fields(&logs[0]),
            s("shop", "shop-5d9c-", "shop", "injected", "ok")
        );
        assert_eq!(
            fields(&logs[1]),
            s("shop", "shop-5d9c-", "shop", "skipped", "host-network")
        );
        assert_eq!(
            fields(&logs[2]),
            s("shop", "shop-5d9c-", "shop", "skipped", "not-selected")
        );
        assert_eq!(logs[3]["result"], "error");
        assert_eq!(logs[3]["reason"], "cache-not-synced");

        let text = metrics.encode();
        for line in [
            r#"mimic_webhook_admissions_total{result="injected",reason="ok"} 1"#,
            r#"mimic_webhook_admissions_total{result="skipped",reason="host-network"} 1"#,
            r#"mimic_webhook_admissions_total{result="skipped",reason="not-selected"} 1"#,
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
    }

    #[tokio::test]
    async fn inj_09_error_before_the_pod_is_known_is_logged_and_counted() {
        let (capture, _guard) = capture_logs();
        let (router, metrics) = setup(true, &[]);
        let (status, _) = call(&router, Method::POST, "/inject/pod", b"nope".to_vec()).await;
        assert_eq!(status, 400);
        let text = metrics.encode();
        assert!(
            text.contains(r#"mimic_webhook_admissions_total{result="error",reason="malformed"} 1"#),
            "{text}"
        );
        let logs = webhook_logs(&capture);
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert_eq!(logs[0]["result"], "error");
    }

    #[tokio::test]
    async fn inj_09_nothing_of_the_pod_spec_leaks_into_logs_or_metrics() {
        let (capture, _guard) = capture_logs();
        let (router, metrics) = setup(true, &[("shop", "w", "shop")]);
        let mut object = pod();
        object["metadata"]["annotations"] = json!({"note": CANARY});
        object["metadata"]["labels"]["secret-label"] = json!(CANARY);
        let mut host = object.clone();
        host["spec"]["hostNetwork"] = json!(true);
        // A body that is JSON but whose pod has a wrongly typed field holding the canary.
        let mut broken = object.clone();
        broken["spec"]["containers"][0]["name"] = json!({"x": CANARY});
        for path in ["/inject/namespace", "/inject/workload", "/inject/pod"] {
            for obj in [&object, &host, &broken] {
                let _ = call(&router, Method::POST, path, review("z", "shop", obj)).await;
            }
        }
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(!logs.is_empty());
        assert!(!logs.contains(CANARY), "log leak:\n{logs}");
        assert!(!metrics.encode().contains(CANARY));
    }
}
