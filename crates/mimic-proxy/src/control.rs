//! Lien avec le contrôleur (spec §5.5 et §7.1) : seul canal par lequel le mode et le
//! snapshot d'un sidecar piloté changent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mimic_core::snapshot::Snapshot;
use serde::{Deserialize, Serialize};

use crate::config::{Mode, ShareConfig};
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Un snapshot ou un segment ne dépasse pas cette taille.
const MAX_BODY: usize = 256 * 1024 * 1024;

/// État souhaité reçu du contrôleur.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub mode: Option<String>,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControllerStatus {
    pub url: String,
    pub desired: Desired,
    pub last_ok_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    pub key: String,
    pub instance: String,
    pub modified_ms: i64,
}

pub struct ControllerClient {
    base: String,
    instance: String,
    token_file: Option<PathBuf>,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl ControllerClient {
    pub fn new(state: &AppState, share: &ShareConfig) -> Result<Self, String> {
        let w = &state.config.workload;
        let ns = if w.namespace.is_empty() {
            "default"
        } else {
            &w.namespace
        };
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(TIMEOUT));
        let builder = hyper_rustls::HttpsConnectorBuilder::new();
        let connector = match &share.ca_file {
            // Seule la CA du contrôleur fait foi, pas les racines système.
            Some(ca) => builder.with_tls_config(crate::tls::pinned_client_config(ca)?),
            None => builder.with_tls_config(crate::tls::client_config(None)?),
        };
        let connector = if share.allow_insecure {
            connector.https_or_http()
        } else {
            connector.https_only()
        }
        .enable_http1()
        .wrap_connector(http);
        Ok(Self {
            base: format!(
                "{}/v1/workloads/{ns}/{}",
                share.url.trim_end_matches('/'),
                w.name
            ),
            instance: state.config.instance.clone(),
            token_file: share.token_file.clone(),
            client: Client::builder(TokioExecutor::new()).build(connector),
        })
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
    ) -> Result<Bytes, String> {
        let mut req = Request::builder()
            .method(method.clone())
            .uri(format!("{}{path}", self.base));
        if let Some(file) = &self.token_file {
            // Relu à chaque appel : le kubelet renouvelle le jeton projeté.
            let token = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("jeton {}: {e}", file.display()))?;
            req = req.header(header::AUTHORIZATION, format!("Bearer {}", token.trim()));
        }
        let (data, content_type) = body.unwrap_or_default();
        if !content_type.is_empty() {
            req = req.header(header::CONTENT_TYPE, content_type);
        }
        let req = req
            .body(Full::new(Bytes::from(data)))
            .map_err(|e| e.to_string())?;
        let resp = tokio::time::timeout(TIMEOUT, self.client.request(req))
            .await
            .map_err(|_| format!("{method} {path}: délai dépassé"))?
            .map_err(|e| format!("{method} {path}: {}", error_chain(&e)))?;
        let status = resp.status();
        let bytes = tokio::time::timeout(
            TIMEOUT * 6,
            Limited::new(resp.into_body(), MAX_BODY).collect(),
        )
        .await
        .map_err(|_| format!("{method} {path}: délai dépassé"))?
        .map_err(|e| format!("{method} {path}: {e}"))?
        .to_bytes();
        if status != StatusCode::OK && status != StatusCode::CREATED {
            return Err(format!(
                "{method} {path}: {status} {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
            ));
        }
        Ok(bytes)
    }

    /// Publie un segment sous son identifiant : une reprise du même segment est sans effet.
    pub async fn put_segment(&self, id: &str, body: Vec<u8>) -> Result<(), String> {
        let path = format!("/segments?instance={}&segment={id}", self.instance);
        self.call(Method::POST, &path, Some((body, "application/x-ndjson")))
            .await
            .map(|_| ())
    }

    /// Segments du workload ; avec `since_ms`, seulement ceux modifiés depuis.
    pub async fn segments(&self, since_ms: Option<i64>) -> Result<Vec<Segment>, String> {
        let path = match since_ms {
            Some(ms) => format!("/segments?since_ms={ms}"),
            None => "/segments".to_string(),
        };
        let b = self.call(Method::GET, &path, None).await?;
        serde_json::from_slice(&b).map_err(|e| format!("liste des segments: {e}"))
    }

    pub async fn segment(&self, key: &str) -> Result<Bytes, String> {
        self.call(Method::GET, &format!("/segments/{key}"), None)
            .await
    }

    pub async fn heartbeat(&self, mode: Mode, snapshot: Option<String>) -> Result<Desired, String> {
        let body = serde_json::json!({
            "instance": self.instance,
            "mode": mode.as_str(),
            "snapshot": snapshot,
        });
        let b = self
            .call(
                Method::POST,
                "/heartbeat",
                Some((body.to_string().into_bytes(), "application/json")),
            )
            .await?;
        serde_json::from_slice(&b).map_err(|e| format!("battement: {e}"))
    }

    pub async fn snapshot(&self, id: &str) -> Result<Snapshot, String> {
        let b = self
            .call(Method::GET, &format!("/snapshots/{id}"), None)
            .await?;
        Snapshot::from_jsonl(&b, id)
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// Fichiers locaux de l'état reçu, pour redémarrer dans le dernier état souhaité.
fn cache_dir(state: &AppState) -> Option<PathBuf> {
    state
        .config
        .store
        .dir
        .as_ref()
        .map(|d| d.join("controller"))
}

/// Recharge le dernier état souhaité et son snapshot depuis `store.dir`.
pub fn restore(state: &AppState) {
    let Some(dir) = cache_dir(state) else {
        return;
    };
    let Ok(text) = std::fs::read(dir.join("desired.json")) else {
        return;
    };
    let Ok(desired) = serde_json::from_slice::<Desired>(&text) else {
        return;
    };
    if let Some(id) = &desired.snapshot
        && let Ok(bytes) = std::fs::read(dir.join("snapshots").join(format!("{id}.jsonl")))
        && let Ok(snap) = Snapshot::from_jsonl(&bytes, id)
    {
        state.set_snapshot(Some(Arc::new(snap)));
    }
    if let Some(m) = desired.mode.as_deref().and_then(Mode::parse) {
        state.set_mode(m);
    }
    tracing::info!(mode = ?desired.mode, snapshot = ?desired.snapshot, "dernier état souhaité rechargé");
    state
        .controller_status
        .lock()
        .expect("verrou du contrôleur")
        .desired = desired;
}

fn save(state: &AppState, desired: &Desired, snap: Option<&Snapshot>) {
    let Some(dir) = cache_dir(state) else {
        return;
    };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("snapshots"))?;
        if let Some(s) = snap {
            std::fs::write(
                dir.join("snapshots").join(format!("{}.jsonl", s.id)),
                s.to_jsonl(),
            )?;
        }
        let tmp = dir.join("desired.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(desired).unwrap_or_default())?;
        std::fs::rename(tmp, dir.join("desired.json"))
    };
    if let Err(e) = write() {
        tracing::warn!(error = %e, "état souhaité non gardé sur disque");
    }
}

/// Battement : annonce l'état du sidecar et applique l'état souhaité en retour.
pub fn spawn(
    state: Arc<AppState>,
    client: Arc<ControllerClient>,
    poll: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = beat(&state, &client).await;
            {
                let mut status = state
                    .controller_status
                    .lock()
                    .expect("verrou du contrôleur");
                match result {
                    Ok(()) => {
                        status.last_ok_ms = Some(crate::share::now_ms());
                        status.last_error = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "battement vers le contrôleur échoué");
                        status.last_error = Some(e);
                    }
                }
            }
            tokio::time::sleep(poll).await;
        }
    })
}

async fn beat(state: &AppState, client: &ControllerClient) -> Result<(), String> {
    let loaded = state.snapshot().map(|s| s.id.clone());
    let desired = client.heartbeat(state.mode(), loaded.clone()).await?;
    let previous = state
        .controller_status
        .lock()
        .expect("verrou du contrôleur")
        .desired
        .clone();
    // Le snapshot d'abord : un passage en mock sert ainsi directement le bon jeu. S'il ne
    // peut pas être téléchargé, le mode est appliqué quand même (le mock sert alors les
    // données locales, rien ne sort) et le téléchargement est retenté au battement suivant.
    let mut fetched = None;
    let mut fetch_error = None;
    match &desired.snapshot {
        Some(id) if loaded.as_ref() != Some(id) => match client.snapshot(id).await {
            Ok(snap) => {
                let snap = Arc::new(snap);
                tracing::info!(snapshot = %id, exchanges = snap.exchanges.len(), "snapshot chargé");
                state.set_snapshot(Some(snap.clone()));
                fetched = Some(snap);
            }
            Err(e) => fetch_error = Some(format!("snapshot {id}: {e}")),
        },
        None if loaded.is_some() => state.set_snapshot(None),
        _ => {}
    }
    match desired.mode.as_deref() {
        Some(m) => match Mode::parse(m) {
            Some(mode) => state.set_mode(mode),
            None => return Err(format!("mode souhaité inconnu: {m}")),
        },
        // Ressource supprimée : retour au mode de la configuration.
        None if previous.mode.is_some() => state.set_mode(state.config.mode),
        None => {}
    }
    if desired != previous || fetched.is_some() {
        save(state, &desired, fetched.as_deref());
        state
            .controller_status
            .lock()
            .expect("verrou du contrôleur")
            .desired = desired;
    }
    fetch_error.map_or(Ok(()), Err)
}
