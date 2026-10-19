//! Authentification des sidecars par jeton de ServiceAccount (spec §7.1).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::PostParams;
use sha2::{Digest, Sha256};

use crate::config::Identity;

/// Audience attendue des jetons projetés.
pub const AUDIENCE: &str = "mimic-controller";
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Missing,
    Invalid,
    /// Jeton valide, mais pour un autre namespace, pod ou workload.
    Forbidden,
    /// API Kubernetes injoignable : l'appel est refusé sans conclure sur le jeton.
    Unavailable(String),
}

impl AuthError {
    pub fn reason(&self) -> &'static str {
        match self {
            AuthError::Missing => "missing",
            AuthError::Invalid => "invalid",
            AuthError::Forbidden => "forbidden",
            AuthError::Unavailable(_) => "unavailable",
        }
    }
}

pub enum Authenticator {
    Static(BTreeMap<String, Identity>),
    Kubernetes(KubeAuth),
}

impl Authenticator {
    pub async fn authenticate(&self, token: &str) -> Result<Identity, AuthError> {
        match self {
            Authenticator::Static(tokens) => tokens.get(token).cloned().ok_or(AuthError::Invalid),
            Authenticator::Kubernetes(k) => k.authenticate(token).await,
        }
    }
}

/// Le pod authentifié peut-il agir sur ce workload, et au nom de cette instance ?
pub fn authorize(
    id: &Identity,
    namespace: &str,
    workload: &str,
    instance: Option<&str>,
) -> Result<(), AuthError> {
    let same_instance = instance.is_none_or(|i| i == id.pod);
    if id.namespace == namespace && id.workload == workload && same_instance {
        Ok(())
    } else {
        Err(AuthError::Forbidden)
    }
}

/// Jeton porté par `Authorization: Bearer`.
pub fn bearer(headers: &http::HeaderMap) -> Option<&str> {
    headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

pub struct KubeAuth {
    client: kube::Client,
    cache: Mutex<BTreeMap<[u8; 32], (Instant, Identity)>>,
}

impl KubeAuth {
    pub fn new(client: kube::Client) -> Self {
        Self {
            client,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    async fn authenticate(&self, token: &str) -> Result<Identity, AuthError> {
        let key: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if let Some((_, id)) = cache.get(&key) {
                return Ok(id.clone());
            }
        }
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: Some(token.to_string()),
                audiences: Some(vec![AUDIENCE.to_string()]),
            },
            ..TokenReview::default()
        };
        let reviewed = Api::<TokenReview>::all(self.client.clone())
            .create(&PostParams::default(), &review)
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?;
        let status = reviewed.status.unwrap_or_default();
        if status.authenticated != Some(true)
            || !status
                .audiences
                .unwrap_or_default()
                .iter()
                .any(|a| a == AUDIENCE)
        {
            return Err(AuthError::Invalid);
        }
        let user = status.user.unwrap_or_default();
        let namespace = user
            .username
            .as_deref()
            .and_then(|u| u.strip_prefix("system:serviceaccount:"))
            .and_then(|rest| rest.split(':').next())
            .ok_or(AuthError::Invalid)?
            .to_string();
        let pod = user
            .extra
            .unwrap_or_default()
            .get("authentication.kubernetes.io/pod-name")
            .and_then(|v| v.first().cloned())
            .ok_or(AuthError::Invalid)?;
        let found = Api::<Pod>::namespaced(self.client.clone(), &namespace)
            .get_opt(&pod)
            .await
            .map_err(|e| AuthError::Unavailable(e.to_string()))?
            .ok_or(AuthError::Invalid)?;
        let workload = workload_of(&found).ok_or(AuthError::Forbidden)?;
        let id = Identity {
            namespace,
            pod,
            workload,
            labels: found.metadata.labels.unwrap_or_default(),
        };
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (Instant::now(), id.clone()));
        Ok(id)
    }
}

/// Nom du workload d'un pod (spec §5.4) : annotation, puis labels usuels. Même règle que
/// l'injecteur, qui fige ce nom dans `MIMIC_WORKLOAD` : annotation sans les espaces qui
/// l'entourent, valeurs vides ignorées.
pub fn workload_of(pod: &Pod) -> Option<String> {
    let meta = &pod.metadata;
    let annotation = meta
        .annotations
        .as_ref()
        .and_then(|a| a.get("mimic.platform.io/workload"))
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());
    let label = |key: &str| {
        meta.labels
            .as_ref()?
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    };
    annotation
        .or_else(|| label("app.kubernetes.io/name"))
        .or_else(|| label("app"))
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity {
            namespace: "shop".into(),
            pod: "checkout-a".into(),
            workload: "checkout".into(),
            ..Identity::default()
        }
    }

    #[test]
    fn sec_03_authorization_rules() {
        assert!(authorize(&id(), "shop", "checkout", Some("checkout-a")).is_ok());
        assert!(authorize(&id(), "shop", "checkout", None).is_ok());
        assert_eq!(
            authorize(&id(), "shop", "checkout", Some("checkout-b")),
            Err(AuthError::Forbidden)
        );
        assert_eq!(
            authorize(&id(), "bank", "checkout", None),
            Err(AuthError::Forbidden)
        );
        assert_eq!(
            authorize(&id(), "shop", "web", None),
            Err(AuthError::Forbidden)
        );
    }

    #[test]
    fn workload_from_annotation_then_labels() {
        let mut pod = Pod::default();
        assert_eq!(workload_of(&pod), None);
        pod.metadata.labels = Some([("app".to_string(), "a".to_string())].into());
        assert_eq!(workload_of(&pod).as_deref(), Some("a"));
        pod.metadata
            .labels
            .as_mut()
            .unwrap()
            .insert("app.kubernetes.io/name".into(), "b".into());
        assert_eq!(workload_of(&pod).as_deref(), Some("b"));
        pod.metadata.annotations =
            Some([("mimic.platform.io/workload".to_string(), "c".to_string())].into());
        assert_eq!(workload_of(&pod).as_deref(), Some("c"));
    }

    #[test]
    fn ctl_34_workload_name_is_trimmed_and_empty_values_skipped() {
        let mut pod = Pod::default();
        pod.metadata.annotations = Some(
            [(
                "mimic.platform.io/workload".to_string(),
                " checkout ".to_string(),
            )]
            .into(),
        );
        assert_eq!(workload_of(&pod).as_deref(), Some("checkout"));
        pod.metadata.annotations =
            Some([("mimic.platform.io/workload".to_string(), "  ".to_string())].into());
        pod.metadata.labels = Some(
            [
                ("app.kubernetes.io/name".to_string(), "web".to_string()),
                ("app".to_string(), "autre".to_string()),
            ]
            .into(),
        );
        assert_eq!(workload_of(&pod).as_deref(), Some("web"));
        pod.metadata.labels = Some(
            [
                ("app.kubernetes.io/name".to_string(), String::new()),
                ("app".to_string(), "autre".to_string()),
            ]
            .into(),
        );
        assert_eq!(workload_of(&pod).as_deref(), Some("autre"));
    }

    #[test]
    fn bearer_parsing() {
        let mut h = http::HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(http::header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(http::header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }
}
