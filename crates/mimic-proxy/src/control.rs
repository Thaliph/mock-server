//! Lien avec le contrôleur (spec §5.5 et §7.1) : seul canal par lequel le mode et le
//! snapshot d'un sidecar piloté changent. Le sidecar télécharge la forme de mock du snapshot
//! (spec §5.6), compressée, et la garde sur disque si `store.dir` est configuré.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mimic_core::snapshot::{MockSnapshot, Snapshot, digest};
use serde::{Deserialize, Serialize};

use crate::config::{InboundCapture, MockWebsocket, Mode, ShareConfig};
use crate::state::AppState;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Un snapshot ne dépasse pas cette taille, compressé comme décompressé.
const MAX_BODY: usize = 256 * 1024 * 1024;

/// Réponse brute du contrôleur.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

/// Forme de mock chargée, et octets effectivement téléchargés pour l'obtenir.
pub struct Fetched {
    pub snapshot: MockSnapshot,
    pub downloaded: usize,
    /// Snapshot complet téléchargé : contrôleur d'une version antérieure (spec §5.6).
    pub full: bool,
}

/// État souhaité reçu du contrôleur.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub mode: Option<String>,
    pub snapshot: Option<String>,
    /// Capture de l'entrant imposée ; absente, celle de la configuration (spec §5.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_inbound: Option<String>,
    /// WebSocket sortant en mock imposé (`relay`, `refuse`) ; absent, celui de la
    /// configuration (spec §3.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock_websocket: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControllerStatus {
    pub url: String,
    pub desired: Desired,
    pub last_ok_ms: Option<u64>,
    pub last_error: Option<String>,
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

    /// Appel attendu en `200` ou `201`.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
    ) -> Result<Bytes, String> {
        let r = self.send(method.clone(), path, body, None).await?;
        if r.status != StatusCode::OK && r.status != StatusCode::CREATED {
            return Err(rejected(&method, path, &r));
        }
        Ok(r.body)
    }

    /// Appel quel que soit le code de la réponse ; `accept` : `Accept-Encoding`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
        accept: Option<&'static str>,
    ) -> Result<Reply, String> {
        let mut req = Request::builder()
            .method(method.clone())
            .uri(format!("{}{path}", self.base));
        if let Some(a) = accept {
            req = req.header(header::ACCEPT_ENCODING, a);
        }
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
        let (status, headers) = (resp.status(), resp.headers().clone());
        let body = tokio::time::timeout(
            TIMEOUT * 6,
            Limited::new(resp.into_body(), MAX_BODY).collect(),
        )
        .await
        .map_err(|_| format!("{method} {path}: délai dépassé"))?
        .map_err(|e| format!("{method} {path}: {e}"))?
        .to_bytes();
        Ok(Reply {
            status,
            headers,
            body,
        })
    }

    /// Publie un segment sous son identifiant : une reprise du même segment est sans effet.
    pub async fn put_segment(&self, id: &str, body: Vec<u8>) -> Result<(), String> {
        let path = format!("/segments?instance={}&segment={id}", self.instance);
        self.call(Method::POST, &path, Some((body, "application/x-ndjson")))
            .await
            .map(|_| ())
    }

    pub async fn heartbeat(
        &self,
        mode: Mode,
        snapshot: Option<String>,
        capture: InboundCapture,
        websocket: MockWebsocket,
    ) -> Result<Desired, String> {
        let body = serde_json::json!({
            "instance": self.instance,
            "mode": mode.as_str(),
            "snapshot": snapshot,
            "capture_inbound": capture.as_str(),
            "mock_websocket": websocket.as_str(),
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

    /// Télécharge la forme de mock du snapshot (spec §5.6), compressée ; un contrôleur d'une
    /// version antérieure (`404`) ne sert que le snapshot complet, dont elle est déduite ici.
    pub async fn snapshot(&self, id: &str) -> Result<Fetched, String> {
        let path = format!("/snapshots/{id}/mock");
        let r = self
            .send(Method::GET, &path, None, Some("zstd, gzip"))
            .await?;
        match r.status {
            StatusCode::OK => Ok(Fetched {
                snapshot: mock_form(&r, id)?,
                downloaded: r.body.len(),
                full: false,
            }),
            StatusCode::NOT_FOUND => {
                let full = self
                    .call(Method::GET, &format!("/snapshots/{id}"), None)
                    .await?;
                Ok(Fetched {
                    snapshot: MockSnapshot::from_snapshot(&Snapshot::from_jsonl(&full, id)?),
                    downloaded: full.len(),
                    full: true,
                })
            }
            _ => Err(rejected(&Method::GET, &path, &r)),
        }
    }
}

fn rejected(method: &Method, path: &str, r: &Reply) -> String {
    format!(
        "{method} {path}: {} {}",
        r.status,
        String::from_utf8_lossy(&r.body[..r.body.len().min(200)])
    )
}

/// Forme de mock reçue : décompressée (bornée), vérifiée contre son `ETag` et l'identifiant
/// du snapshot demandé.
fn mock_form(r: &Reply, id: &str) -> Result<MockSnapshot, String> {
    let encoding = r
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    let json = match encoding.as_str() {
        "identity" | "" => r.body.to_vec(),
        e => crate::capture::decode(e, &r.body, MAX_BODY)
            .ok_or_else(|| format!("forme de mock du snapshot {id}: encodage {e} illisible"))?,
    };
    if let Some(etag) = r.headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
        let expected = etag.trim().trim_start_matches("W/").trim_matches('"');
        let got = digest(&json);
        if got != expected {
            return Err(format!(
                "forme de mock du snapshot {id} altérée (empreinte {got}, ETag {expected})"
            ));
        }
    }
    MockSnapshot::from_json(&json, id)
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
        && let Some(snap) = cached_snapshot(&dir.join("snapshots"), id)
    {
        state.set_snapshot(Some(Arc::new(snap)));
    }
    if let Some(m) = desired.mode.as_deref().and_then(Mode::parse) {
        state.set_mode(m);
    }
    if let Some(c) = desired
        .capture_inbound
        .as_deref()
        .and_then(InboundCapture::parse)
    {
        state.set_inbound_capture(c);
    }
    if let Some(w) = desired
        .mock_websocket
        .as_deref()
        .and_then(MockWebsocket::parse)
    {
        state.set_mock_websocket(w);
    }
    tracing::info!(mode = ?desired.mode, snapshot = ?desired.snapshot, "dernier état souhaité rechargé");
    state
        .controller_status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .desired = desired;
}

/// Forme de mock gardée sur disque ; à défaut, un snapshot complet gardé par une version
/// antérieure du sidecar, dont elle est déduite.
fn cached_snapshot(dir: &std::path::Path, id: &str) -> Option<MockSnapshot> {
    if let Ok(bytes) = std::fs::read(dir.join(format!("{id}.mock.json"))) {
        return MockSnapshot::from_json(&bytes, id).ok();
    }
    let bytes = std::fs::read(dir.join(format!("{id}.jsonl"))).ok()?;
    let full = Snapshot::from_jsonl(&bytes, id).ok()?;
    Some(MockSnapshot::from_snapshot(&full))
}

fn save(state: &AppState, desired: &Desired, snap: Option<&MockSnapshot>) {
    let Some(dir) = cache_dir(state) else {
        return;
    };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join("snapshots"))?;
        if let Some(s) = snap {
            // Échanges du snapshot, déjà redactés par les sidecars qui les ont publiés (§7).
            std::fs::write(
                dir.join("snapshots").join(format!("{}.mock.json", s.id)),
                s.to_json(),
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
                    .unwrap_or_else(PoisonError::into_inner);
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
    let desired = client
        .heartbeat(
            state.mode(),
            loaded.clone(),
            state.inbound_capture(),
            state.mock_websocket(),
        )
        .await?;
    let previous = state
        .controller_status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .desired
        .clone();
    // Le snapshot d'abord : un passage en mock sert ainsi directement le bon jeu. S'il ne
    // peut pas être téléchargé, le mode est appliqué quand même (le mock sert alors les
    // données locales, rien ne sort) et le téléchargement est retenté au battement suivant.
    let mut fetched = None;
    let mut fetch_error = None;
    match &desired.snapshot {
        Some(id) if loaded.as_ref() != Some(id) => match client.snapshot(id).await {
            Ok(f) => {
                let snap = Arc::new(f.snapshot);
                tracing::info!(
                    snapshot = %id,
                    exchanges = snap.count,
                    records = snap.records.len(),
                    downloaded = f.downloaded,
                    full = f.full,
                    "snapshot chargé"
                );
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
    match desired.capture_inbound.as_deref() {
        Some(c) => match InboundCapture::parse(c) {
            Some(c) => state.set_inbound_capture(c),
            None => return Err(format!("capture de l'entrant inconnue: {c}")),
        },
        // Plus de capture imposée : celle de la configuration.
        None => state.set_inbound_capture(state.config.capture.inbound),
    }
    match desired.mock_websocket.as_deref() {
        Some(w) => match MockWebsocket::parse(w) {
            Some(w) => state.set_mock_websocket(w),
            None => return Err(format!("réglage WebSocket inconnu: {w}")),
        },
        // Plus de réglage imposé : celui de la configuration.
        None => state.set_mock_websocket(state.config.mock.websocket),
    }
    if desired != previous || fetched.is_some() {
        save(state, &desired, fetched.as_deref());
        state
            .controller_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .desired = desired;
    }
    fetch_error.map_or(Ok(()), Err)
}
