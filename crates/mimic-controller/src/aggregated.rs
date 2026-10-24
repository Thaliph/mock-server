//! API agrégée de lecture `data.mimic.platform.io/v1alpha1` (spec §9.2c).
//!
//! L'API server authentifie l'utilisateur, puis relaie sa requête avec son certificat de
//! front proxy (exigé par le serveur TLS) et l'identité dans `X-Remote-User`,
//! `X-Remote-Group` et `X-Remote-Extra-*`. Chaque route est autorisée par une
//! `SubjectAccessReview` sur le namespace et le workload demandés, puis servie par la route
//! équivalente de l'interface (§9.2b), même corps et mêmes codes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::future::BoxFuture;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::ConfigMap;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::{Ctl, valid_name};

pub const GROUP: &str = "data.mimic.platform.io";
pub const VERSION: &str = "v1alpha1";
/// Préfixe des routes de l'API agrégée.
pub const PREFIX: &str = "/apis/data.mimic.platform.io/v1alpha1";

/// Sous-ressources de `workloads`, lues par `mimicctl`.
const SUBRESOURCES: [&str; 4] = ["operations", "openapi", "requests", "snapshots"];
/// Durée de vie d'une décision d'autorisation.
const DECISION_TTL: Duration = Duration::from_secs(10);
/// Décisions gardées au plus ; au-delà, le cache repart de zéro.
const MAX_DECISIONS: usize = 10_000;

/// Accès demandé par un utilisateur, tel qu'une `SubjectAccessReview` le décrit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Access {
    pub user: String,
    pub groups: Vec<String>,
    pub extra: BTreeMap<String, Vec<String>>,
    /// `get` ou `list`.
    pub verb: String,
    pub namespace: Option<String>,
    /// Nom du workload.
    pub name: Option<String>,
    pub subresource: Option<String>,
}

/// Décide d'un accès : `SubjectAccessReview` en cluster, simulée dans les tests.
pub trait Authorizer: Send + Sync {
    /// `Err` : décision impossible (API server injoignable).
    fn allowed<'a>(&'a self, access: &'a Access) -> BoxFuture<'a, Result<bool, String>>;
}

/// Autorisation par `SubjectAccessReview`.
pub struct KubeAuthorizer {
    client: kube::Client,
}

impl KubeAuthorizer {
    pub fn new(client: kube::Client) -> Self {
        Self { client }
    }
}

impl Authorizer for KubeAuthorizer {
    fn allowed<'a>(&'a self, access: &'a Access) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let review = SubjectAccessReview {
                spec: SubjectAccessReviewSpec {
                    user: Some(access.user.clone()),
                    groups: Some(access.groups.clone()),
                    extra: (!access.extra.is_empty()).then(|| access.extra.clone()),
                    resource_attributes: Some(ResourceAttributes {
                        group: Some(GROUP.into()),
                        version: Some(VERSION.into()),
                        resource: Some("workloads".into()),
                        verb: Some(access.verb.clone()),
                        namespace: access.namespace.clone(),
                        name: access.name.clone(),
                        subresource: access.subresource.clone(),
                        ..ResourceAttributes::default()
                    }),
                    ..SubjectAccessReviewSpec::default()
                },
                ..SubjectAccessReview::default()
            };
            let reviewed = kube::Api::<SubjectAccessReview>::all(self.client.clone())
                .create(&kube::api::PostParams::default(), &review)
                .await
                .map_err(|e| e.to_string())?;
            Ok(reviewed.status.is_some_and(|s| s.allowed))
        })
    }
}

/// CA des certificats clients et noms communs acceptés, lus dans
/// `kube-system/extension-apiserver-authentication`.
pub async fn front_proxy_ca(client: &kube::Client) -> Result<(String, Vec<String>), String> {
    let cm = kube::Api::<ConfigMap>::namespaced(client.clone(), "kube-system")
        .get("extension-apiserver-authentication")
        .await
        .map_err(|e| format!("kube-system/extension-apiserver-authentication: {e}"))?;
    let data = cm.data.unwrap_or_default();
    let ca = data
        .get("requestheader-client-ca-file")
        .cloned()
        .ok_or("requestheader-client-ca-file absent : l'API server n'a pas de front proxy")?;
    let names = match data.get("requestheader-allowed-names") {
        Some(v) if !v.trim().is_empty() => serde_json::from_str(v)
            .map_err(|e| format!("requestheader-allowed-names illisible: {e}"))?,
        _ => vec![],
    };
    Ok((ca, names))
}

/// Ce qu'une route de l'API agrégée demande : la découverte, ou un accès et la route de
/// l'interface qui la sert.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Discovery,
    Read {
        verb: &'static str,
        namespace: Option<String>,
        name: Option<String>,
        subresource: Option<String>,
        /// Chemin de l'interface, query comprise.
        ui: String,
        /// Liste réduite aux workloads de ce namespace.
        only_namespace: Option<String>,
    },
}

/// Route de l'API agrégée pour un chemin (préfixe compris) et sa query.
pub fn route(path: &str, query: Option<&str>) -> Option<Route> {
    let rest = path.strip_prefix(PREFIX)?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Some(Route::Discovery);
    }
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    let list = |namespace: Option<&str>| Route::Read {
        verb: "list",
        namespace: namespace.map(String::from),
        name: None,
        subresource: None,
        ui: "/v1/workloads".into(),
        only_namespace: namespace.map(String::from),
    };
    match parts.as_slice() {
        ["workloads"] => Some(list(None)),
        ["namespaces", ns, "workloads"] if valid_name(ns) => Some(list(Some(ns))),
        ["namespaces", ns, "workloads", wl, sub, tail @ ..]
            if valid_name(ns) && valid_name(wl) && SUBRESOURCES.contains(sub) =>
        {
            let tail = tail.iter().map(|t| format!("/{t}")).collect::<String>();
            Some(Route::Read {
                verb: "get",
                namespace: Some(ns.to_string()),
                name: Some(wl.to_string()),
                subresource: Some(sub.to_string()),
                ui: format!("/v1/workloads/{ns}/{wl}/{sub}{tail}{query}"),
                only_namespace: None,
            })
        }
        _ => None,
    }
}

/// Chemin de l'API agrégée qui lit la même chose qu'un chemin de l'interface ; `None` pour
/// une écriture ou une route que l'API agrégée ne sert pas. `/v1/workloads?namespace=<ns>`
/// désigne la liste d'un namespace.
pub fn aggregated_path(ui: &str) -> Option<String> {
    let (path, query) = match ui.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (ui, None),
    };
    let rest = path.strip_prefix("/v1/workloads")?;
    if rest.is_empty() {
        return match query.and_then(|q| q.strip_prefix("namespace=")) {
            Some(ns) if valid_name(ns) => Some(format!("{PREFIX}/namespaces/{ns}/workloads")),
            Some(_) => None,
            None => Some(format!("{PREFIX}/workloads")),
        };
    }
    let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    let [ns, wl, sub, tail @ ..] = parts.as_slice() else {
        return None;
    };
    // Écritures du cycle de vie des snapshots : jamais par l'API agrégée.
    if !SUBRESOURCES.contains(sub)
        || (*sub == "snapshots" && (tail == ["gc"] || tail.last() == Some(&"pin")))
    {
        return None;
    }
    let path = format!(
        "{PREFIX}/namespaces/{ns}/workloads/{wl}/{}",
        parts[2..].join("/")
    );
    Some(match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    })
}

/// Utilisateur authentifié par l'API server.
struct User {
    name: String,
    groups: Vec<String>,
    extra: BTreeMap<String, Vec<String>>,
}

/// Identité posée par le front proxy, ou `None` sans `X-Remote-User`.
fn identity(headers: &HeaderMap) -> Option<User> {
    let text = |v: &axum::http::HeaderValue| v.to_str().ok().map(String::from);
    let name = headers.get("x-remote-user").and_then(text)?;
    let groups = headers
        .get_all("x-remote-group")
        .iter()
        .filter_map(text)
        .collect();
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        if let (Some(key), Some(v)) = (name.as_str().strip_prefix("x-remote-extra-"), text(value)) {
            extra.entry(percent_decode(key)).or_default().push(v);
        }
    }
    Some(User {
        name,
        groups,
        extra,
    })
}

/// Clé d'un extra : l'API server l'encode en pourcentage (`%2F` pour `/`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `Status` Kubernetes d'un refus.
fn status(code: StatusCode, reason: &str, message: String) -> Response {
    (
        code,
        axum::Json(json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "reason": reason,
            "message": message,
            "code": code.as_u16(),
        })),
    )
        .into_response()
}

/// Liste des ressources du groupe : découverte et sonde de disponibilité de l'APIService.
fn discovery() -> Response {
    let resource = |name: &str| {
        json!({
            "name": name,
            "singularName": if name == "workloads" { "workload" } else { "" },
            "namespaced": true,
            "kind": "MimicData",
            "verbs": if name == "workloads" { json!(["get", "list"]) } else { json!(["get"]) },
        })
    };
    let mut resources = vec![resource("workloads")];
    resources.extend(
        SUBRESOURCES
            .iter()
            .map(|s| resource(&format!("workloads/{s}"))),
    );
    axum::Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": format!("{GROUP}/{VERSION}"),
        "resources": resources,
    }))
    .into_response()
}

struct Api {
    ui: Router,
    authorizer: Arc<dyn Authorizer>,
    decisions: Mutex<BTreeMap<Access, (bool, Instant)>>,
}

impl Api {
    /// Décision pour un accès, gardée `DECISION_TTL` ; une erreur n'est pas gardée.
    async fn decide(&self, access: &Access) -> Result<bool, String> {
        let now = Instant::now();
        let cached = self
            .decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(access)
            .filter(|(_, at)| now.duration_since(*at) < DECISION_TTL)
            .map(|(allowed, _)| *allowed);
        if let Some(allowed) = cached {
            return Ok(allowed);
        }
        let allowed = self.authorizer.allowed(access).await?;
        let mut decisions = self
            .decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if decisions.len() >= MAX_DECISIONS {
            decisions.clear();
        }
        decisions.insert(access.clone(), (allowed, now));
        Ok(allowed)
    }
}

/// Routeur de l'API agrégée, servi sur `aggregated.listen` avec certificat client exigé.
pub fn router(ctl: Arc<Ctl>, authorizer: Arc<dyn Authorizer>) -> Router {
    let api = Arc::new(Api {
        ui: crate::ui::router(ctl),
        authorizer,
        decisions: Mutex::new(BTreeMap::new()),
    });
    Router::new().fallback(handle).with_state(api)
}

async fn handle(State(api): State<Arc<Api>>, req: Request) -> Response {
    let Some(route) = route(req.uri().path(), req.uri().query()) else {
        return status(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("{} : route inconnue", req.uri().path()),
        );
    };
    if req.method() != Method::GET {
        return status(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "API de lecture seule : GET uniquement".into(),
        );
    }
    let Route::Read {
        verb,
        namespace,
        name,
        subresource,
        ui,
        only_namespace,
    } = route
    else {
        return discovery();
    };
    let Some(user) = identity(req.headers()) else {
        return status(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "X-Remote-User absent".into(),
        );
    };
    let access = Access {
        user: user.name,
        groups: user.groups,
        extra: user.extra,
        verb: verb.into(),
        namespace,
        name,
        subresource,
    };
    match api.decide(&access).await {
        Ok(true) => {}
        Ok(false) => {
            let what = match (&access.namespace, &access.name, &access.subresource) {
                (Some(ns), Some(wl), Some(sub)) => format!("{verb} workloads/{sub} {ns}/{wl}"),
                (Some(ns), _, _) => format!("{verb} workloads dans {ns}"),
                _ => format!("{verb} workloads sur le cluster"),
            };
            return status(
                StatusCode::FORBIDDEN,
                "Forbidden",
                format!(
                    "{} ne peut pas {what} ({GROUP}) : demander un RoleBinding du ClusterRole mimic-data-viewer",
                    access.user
                ),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "SubjectAccessReview impossible");
            return status(
                StatusCode::SERVICE_UNAVAILABLE,
                "ServiceUnavailable",
                format!("autorisation impossible : {e}"),
            );
        }
    }
    let inner = match Request::builder()
        .method(Method::GET)
        .uri(&ui)
        .body(Body::empty())
    {
        Ok(r) => r,
        Err(e) => return status(StatusCode::BAD_REQUEST, "BadRequest", e.to_string()),
    };
    let resp = match api.ui.clone().oneshot(inner).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    match only_namespace {
        Some(ns) if resp.status().is_success() => only(resp, &ns).await,
        _ => resp,
    }
}

/// Liste des workloads réduite à ceux du namespace `ns`.
async fn only(resp: Response, ns: &str) -> Response {
    let (parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(Value::Array(list)) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let kept: Vec<Value> = list.into_iter().filter(|w| w["namespace"] == ns).collect();
    let mut resp = axum::Json(kept).into_response();
    *resp.status_mut() = parts.status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    resp
}
