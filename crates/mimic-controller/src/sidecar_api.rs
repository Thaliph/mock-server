//! Sidecar API (spec §7.2): sidecars authenticate with their projected service-account token,
//! checked with a `TokenReview`, and ask for an intermediate CA certificate.
//!
//! Neither the token nor the posted key is ever logged or put in a metric label.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use futures::StreamExt;
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};
use mimic_core::identity::pod_of_deployment;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OnceCell;

use crate::tls_ca::{SeedSlot, derive_root};

/// Audience of the projected token (spec §7.1).
pub const AUDIENCE: &str = "mimic.platform.io";
/// Largest request body accepted.
pub const MAX_BODY: usize = 64 * 1024;
const ENDPOINT: &str = "/v1/tls/intermediate";
/// Claim holding the pod a service-account token is bound to.
const POD_NAME_CLAIM: &str = "authentication.kubernetes.io/pod-name";

type Labels = Vec<(String, String)>;

/// `mimic_sidecar_api_requests_total{endpoint,result}`.
#[derive(Clone, Default)]
pub struct ApiMetrics {
    requests: Family<Labels, Counter>,
}

impl ApiMetrics {
    pub fn register(registry: &mut Registry) -> ApiMetrics {
        let metrics = ApiMetrics::default();
        registry.register(
            "sidecar_api_requests",
            "Requests answered by the sidecar API",
            metrics.requests.clone(),
        );
        metrics
    }
}

/// What a `TokenReview` says about a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub authenticated: bool,
    /// `system:serviceaccount:<namespace>:<name>`.
    pub username: String,
    /// The pod the token is bound to.
    pub pod: Option<String>,
}

pub type ReviewFuture<'a> = Pin<Box<dyn Future<Output = Result<Review, String>> + Send + 'a>>;

/// Validates tokens; an error means the answer could not be obtained.
pub trait Reviewer: Send + Sync {
    fn review<'a>(&'a self, token: &'a str) -> ReviewFuture<'a>;
}

/// `TokenReview` against the Kubernetes API server.
pub struct KubeReviewer {
    client: Arc<OnceCell<kube::Client>>,
}

impl KubeReviewer {
    /// `client` is set once the cluster client is available; reviews fail until then.
    pub fn new(client: Arc<OnceCell<kube::Client>>) -> KubeReviewer {
        KubeReviewer { client }
    }
}

impl Reviewer for KubeReviewer {
    fn review<'a>(&'a self, token: &'a str) -> ReviewFuture<'a> {
        Box::pin(async move {
            let review = TokenReview {
                spec: TokenReviewSpec {
                    token: Some(token.to_string()),
                    audiences: Some(vec![AUDIENCE.to_string()]),
                },
                ..TokenReview::default()
            };
            let client = self.client.get().ok_or("no Kubernetes client yet")?;
            let answer = Api::<TokenReview>::all(client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|e| format!("token review: {e}"))?;
            let status = answer.status.unwrap_or_default();
            // The API server answers for the audiences it validated: ours must be among them.
            let audience = status
                .audiences
                .as_ref()
                .is_some_and(|audiences| audiences.iter().any(|a| a == AUDIENCE));
            let user = status.user.unwrap_or_default();
            let pod = user
                .extra
                .as_ref()
                .and_then(|extra| extra.get(POD_NAME_CLAIM))
                .and_then(|values| values.first())
                .cloned();
            Ok(Review {
                authenticated: status.authenticated.unwrap_or(false) && audience,
                username: user.username.unwrap_or_default(),
                pod,
            })
        })
    }
}

/// What the endpoints need.
pub struct SidecarApi {
    pub seed: Arc<SeedSlot>,
    pub reviewer: Arc<dyn Reviewer>,
    pub metrics: ApiMetrics,
}

pub fn router(api: Arc<SidecarApi>) -> Router {
    Router::new()
        .route(
            ENDPOINT,
            post(
                |State(api): State<Arc<SidecarApi>>, headers: HeaderMap, body: Body| async move {
                    api.intermediate(&headers, body).await
                },
            ),
        )
        .with_state(api)
}

/// Why a request was refused: the status and the `result` label of the log line and metric.
struct Refusal {
    status: StatusCode,
    result: &'static str,
}

const UNAUTHENTICATED: Refusal = Refusal {
    status: StatusCode::UNAUTHORIZED,
    result: "unauthenticated",
};
const FORBIDDEN: Refusal = Refusal {
    status: StatusCode::FORBIDDEN,
    result: "forbidden",
};
const INVALID: Refusal = Refusal {
    status: StatusCode::BAD_REQUEST,
    result: "invalid",
};
const TOO_LARGE: Refusal = Refusal {
    status: StatusCode::PAYLOAD_TOO_LARGE,
    result: "invalid",
};
const UNAVAILABLE: Refusal = Refusal {
    status: StatusCode::SERVICE_UNAVAILABLE,
    result: "unavailable",
};

/// What is known of the caller, for the log line.
#[derive(Default)]
struct Caller {
    namespace: String,
    pod: String,
    deployment: String,
}

#[derive(Deserialize)]
struct IntermediateRequest {
    deployment: String,
    public_key: String,
}

impl SidecarApi {
    async fn intermediate(&self, headers: &HeaderMap, body: Body) -> Response {
        let mut caller = Caller::default();
        let outcome = self.authorize_and_sign(headers, body, &mut caller).await;
        let result = outcome.as_ref().map_or_else(|r| r.result, |_| "ok");
        self.metrics
            .requests
            .get_or_create(&vec![
                ("endpoint".to_string(), ENDPOINT.to_string()),
                ("result".to_string(), result.to_string()),
            ])
            .inc();
        tracing::info!(
            target: "sidecar_api",
            endpoint = ENDPOINT,
            namespace = caller.namespace,
            pod = caller.pod,
            deployment = caller.deployment,
            result,
            "sidecar api request"
        );
        match outcome {
            Ok(certificate) => Json(json!({ "certificate": certificate })).into_response(),
            Err(refusal) => refusal.status.into_response(),
        }
    }

    async fn authorize_and_sign(
        &self,
        headers: &HeaderMap,
        body: Body,
        caller: &mut Caller,
    ) -> Result<String, Refusal> {
        let token = bearer(headers).ok_or(UNAUTHENTICATED)?;
        let review = self.reviewer.review(token).await.map_err(|e| {
            tracing::warn!(error = %e, "token review failed");
            UNAVAILABLE
        })?;
        if !review.authenticated {
            return Err(UNAUTHENTICATED);
        }
        let namespace = review
            .username
            .strip_prefix("system:serviceaccount:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(namespace, _)| namespace)
            .filter(|namespace| !namespace.is_empty())
            .ok_or(FORBIDDEN)?;
        caller.namespace = namespace.to_string();
        caller.pod = review.pod.clone().unwrap_or_default();

        let bytes = read_body(body).await?;
        // Parse errors quote the offending value: they are never logged or returned.
        let request: IntermediateRequest = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
        if !valid_name(&request.deployment) {
            return Err(INVALID);
        }
        caller.deployment = request.deployment.clone();
        let public_key = base64::engine::general_purpose::STANDARD
            .decode(request.public_key.as_bytes())
            .map_err(|_| INVALID)?;

        let pod = review.pod.as_deref().ok_or(FORBIDDEN)?;
        if !pod_of_deployment(pod, &request.deployment) {
            return Err(FORBIDDEN);
        }
        let seed = self.seed.get().ok_or(UNAVAILABLE)?;
        let root = derive_root(&seed, namespace, &request.deployment).map_err(|e| {
            tracing::error!(error = %e, "root derivation failed");
            UNAVAILABLE
        })?;
        root.sign_intermediate(&public_key, SystemTime::now())
            .map_err(|_| INVALID)
    }
}

/// The token of an `Authorization: Bearer` header, if any.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

/// A Kubernetes object name: it is logged, so nothing else gets through.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Reads the body, refusing more than [`MAX_BODY`] bytes.
async fn read_body(body: Body) -> Result<Vec<u8>, Refusal> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| INVALID)?;
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls_ca::{Seed, derive_root};
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use mimic_runtime::metrics::Metrics;
    use rcgen::{KeyPair, PublicKeyData};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::SystemTime;
    use tower::ServiceExt;
    use x509_parser::pem::parse_x509_pem;

    const CANARY: &str = "canary-token-7f3a91";
    const POD: &str = "pay-7d9f8b6c4d-x2k9p";

    type Reviews = Arc<Mutex<Vec<Value>>>;

    /// An API server answering `TokenReview` by token: `ok` is the pod `POD` of namespace
    /// `shop`, `other-pod` another deployment's, `no-claim` has no bound pod, `wrong-audience`
    /// is accepted but for another audience, `boom` makes the call fail.
    fn api_server() -> (kube::Client, Reviews) {
        let reviews = Reviews::default();
        let seen = reviews.clone();
        let service = tower::service_fn(move |request: Request<kube::client::Body>| {
            let seen = seen.clone();
            async move {
                assert_eq!(
                    request.uri().path(),
                    "/apis/authentication.k8s.io/v1/tokenreviews"
                );
                let bytes = request.into_body().collect_bytes().await.unwrap();
                let review: Value = serde_json::from_slice(&bytes).unwrap();
                seen.lock().unwrap().push(review.clone());
                let token = review["spec"]["token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let user = |pod: Option<&str>| {
                    let mut user = json!({"username": "system:serviceaccount:shop:default"});
                    if let Some(pod) = pod {
                        user["extra"] = json!({POD_NAME_CLAIM: [pod]});
                    }
                    user
                };
                let (code, status) = match token.as_str() {
                    t if t.starts_with("ok") => (
                        200,
                        json!({"authenticated": true, "user": user(Some(POD)),
                               "audiences": [AUDIENCE]}),
                    ),
                    "other-pod" => (
                        200,
                        json!({"authenticated": true,
                               "user": user(Some("web-7d9f8b6c4d-x2k9p")),
                               "audiences": [AUDIENCE]}),
                    ),
                    "no-claim" => (
                        200,
                        json!({"authenticated": true, "user": user(None),
                               "audiences": [AUDIENCE]}),
                    ),
                    "wrong-audience" => (
                        200,
                        json!({"authenticated": true, "user": user(Some(POD)),
                               "audiences": ["https://kubernetes.default.svc"]}),
                    ),
                    "boom" => (
                        500,
                        json!({"kind": "Status", "apiVersion": "v1",
                               "status": "Failure", "message": "boom", "code": 500}),
                    ),
                    _ => (
                        200,
                        json!({"authenticated": false, "error": "invalid bearer token"}),
                    ),
                };
                let body = if code == 200 {
                    json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview",
                           "metadata": {}, "spec": {}, "status": status})
                } else {
                    status
                };
                Ok::<_, std::convert::Infallible>(
                    axum::http::Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
            }
        });
        (kube::Client::new(service, "default"), reviews)
    }

    fn seed() -> Arc<SeedSlot> {
        let slot = SeedSlot::new();
        slot.set(Seed::generate(SystemTime::now()));
        Arc::new(slot)
    }

    struct Setup {
        router: Router,
        metrics: Arc<Metrics>,
        reviews: Reviews,
        seed: Arc<SeedSlot>,
    }

    fn setup() -> Setup {
        setup_with(seed())
    }

    fn setup_with(seed: Arc<SeedSlot>) -> Setup {
        let (client, reviews) = api_server();
        let mut metrics = Metrics::new("controller", "test");
        let api = Arc::new(SidecarApi {
            seed: seed.clone(),
            reviewer: Arc::new(KubeReviewer::new(Arc::new(OnceCell::new_with(Some(
                client,
            ))))),
            metrics: ApiMetrics::register(metrics.registry_mut()),
        });
        Setup {
            router: router(api),
            metrics: Arc::new(metrics),
            reviews,
            seed,
        }
    }

    fn body(deployment: &str, key: &KeyPair) -> Vec<u8> {
        use base64::Engine;
        serde_json::to_vec(&json!({
            "deployment": deployment,
            "public_key": base64::engine::general_purpose::STANDARD
                .encode(key.subject_public_key_info()),
        }))
        .unwrap()
    }

    async fn call(
        router: &Router,
        method: Method,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (u16, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(ENDPOINT);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn post_status(s: &Setup, token: Option<&str>, body: Vec<u8>) -> u16 {
        call(&s.router, Method::POST, token, body).await.0
    }

    #[tokio::test]
    async fn tls_09_a_valid_token_gets_an_intermediate_chaining_to_the_deployment_root() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let (status, bytes) = call(&s.router, Method::POST, Some("ok"), body("pay", &key)).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let answer: Value = serde_json::from_slice(&bytes).unwrap();
        let pem = answer["certificate"].as_str().unwrap();
        let der = parse_x509_pem(pem.as_bytes()).unwrap().1.contents;
        let (_, inter) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(inter.public_key().raw, &key.subject_public_key_info()[..]);
        // Signed by the root of (token namespace, requested deployment), not by another.
        let verifies_under = |ns: &str, dep: &str| {
            let root = derive_root(&s.seed.get().unwrap(), ns, dep).unwrap();
            let der = parse_x509_pem(root.certificate_pem().as_bytes())
                .unwrap()
                .1
                .contents;
            let (_, root) = x509_parser::parse_x509_certificate(&der).unwrap();
            inter.verify_signature(Some(root.public_key())).is_ok()
        };
        assert!(verifies_under("shop", "pay"));
        assert!(!verifies_under("other", "pay"));
        assert!(!verifies_under("shop", "web"));
    }

    #[tokio::test]
    async fn tls_09_the_token_review_asks_for_the_mimic_audience() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
        let reviews = s.reviews.lock().unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["spec"]["token"], "ok");
        assert_eq!(reviews[0]["spec"]["audiences"], json!([AUDIENCE]));
    }

    #[tokio::test]
    async fn tls_09_missing_invalid_and_foreign_audience_tokens_get_401() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        for token in [None, Some(""), Some("bad"), Some("wrong-audience")] {
            assert_eq!(
                post_status(&s, token, body("pay", &key)).await,
                401,
                "{token:?}"
            );
        }
        // Without a token the API server is not asked.
        let asked = s.reviews.lock().unwrap().len();
        assert_eq!(asked, 2, "bad and wrong-audience only");
        // Not a bearer scheme.
        let request = Request::builder()
            .method(Method::POST)
            .uri(ENDPOINT)
            .header("authorization", "Basic abc")
            .body(Body::from(body("pay", &key)))
            .unwrap();
        let response = s.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 401);
    }

    #[tokio::test]
    async fn tls_09_a_pod_of_another_deployment_or_without_claim_gets_403() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        // The token is bound to a `pay` pod.
        assert_eq!(post_status(&s, Some("ok"), body("web", &key)).await, 403);
        assert_eq!(post_status(&s, Some("ok"), body("pa", &key)).await, 403);
        assert_eq!(
            post_status(&s, Some("other-pod"), body("pay", &key)).await,
            403
        );
        assert_eq!(
            post_status(&s, Some("no-claim"), body("pay", &key)).await,
            403
        );
    }

    #[tokio::test]
    async fn tls_09_malformed_bodies_get_400_oversized_413_other_methods_405() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        for bad in [
            b"nope".to_vec(),
            b"{}".to_vec(),
            serde_json::to_vec(&json!({"deployment": "pay", "public_key": "!!!"})).unwrap(),
            serde_json::to_vec(&json!({"deployment": "pay", "public_key": "AAAA"})).unwrap(),
            serde_json::to_vec(&json!({"deployment": "", "public_key": "AAAA"})).unwrap(),
            body("pay", &p384),
        ] {
            assert_eq!(post_status(&s, Some("ok"), bad).await, 400);
        }
        let big = vec![b' '; MAX_BODY + 1];
        assert_eq!(post_status(&s, Some("ok"), big).await, 413);
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            let (status, _) = call(&s.router, method, Some("ok"), body("pay", &key)).await;
            assert_eq!(status, 405);
        }
    }

    #[tokio::test]
    async fn tls_09_a_failing_token_review_or_a_missing_seed_gets_503() {
        let s = setup();
        let key = KeyPair::generate().unwrap();
        assert_eq!(post_status(&s, Some("boom"), body("pay", &key)).await, 503);
        let unseeded = setup_with(Arc::new(SeedSlot::new()));
        assert_eq!(
            post_status(&unseeded, Some("ok"), body("pay", &key)).await,
            503
        );
    }

    #[tokio::test]
    async fn tls_10_one_log_line_and_one_counter_per_request_and_no_secret() {
        let (capture, _guard) = crate::webhook::tests::capture_logs();
        let s = setup();
        let key = KeyPair::generate().unwrap();
        let key_b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(key.subject_public_key_info())
        };
        assert_eq!(post_status(&s, Some("ok"), body("pay", &key)).await, 200);
        assert_eq!(post_status(&s, Some("ok"), body("web", &key)).await, 403);
        assert_eq!(post_status(&s, Some(CANARY), body("pay", &key)).await, 401);
        assert_eq!(post_status(&s, Some("ok"), b"nope".to_vec()).await, 400);
        assert_eq!(post_status(&s, Some("boom"), body("pay", &key)).await, 503);

        let raw = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let logs: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["target"] == "sidecar_api")
            .collect();
        assert_eq!(logs.len(), 5, "{logs:?}");
        let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
        let line = |i: usize| {
            ["endpoint", "namespace", "pod", "deployment", "result"].map(|k| field(&logs[i], k))
        };
        assert_eq!(line(0), [ENDPOINT, "shop", POD, "pay", "ok"]);
        assert_eq!(line(1), [ENDPOINT, "shop", POD, "web", "forbidden"]);
        assert_eq!(field(&logs[2], "result"), "unauthenticated");
        assert_eq!(field(&logs[3], "result"), "invalid");
        assert_eq!(field(&logs[4], "result"), "unavailable");

        let text = s.metrics.encode();
        for line in [
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="ok"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="forbidden"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="unauthenticated"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="invalid"} 1"#,
            r#"mimic_sidecar_api_requests_total{endpoint="/v1/tls/intermediate",result="unavailable"} 1"#,
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        for secret in [CANARY, key_b64.as_str()] {
            assert!(!raw.contains(secret), "log leak:\n{raw}");
            assert!(!text.contains(secret), "metric leak:\n{text}");
        }
    }
}
