//! Lien avec le contrôleur (spec §5.5 et §7.1) : seul canal par lequel le mode et le
//! snapshot d'un sidecar piloté changent. Le sidecar télécharge la forme de mock du snapshot
//! (spec §5.6), compressée, et la garde sur disque si `store.dir` est configuré. Le premier
//! état souhaité appliqué ouvre la barrière de démarrage de l'application (spec §5.5.5).

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mimic_core::snapshot::{MockSnapshot, Snapshot, SnapshotEntry, digest, to_purge};
use mimic_core::startup::Phase;
use mimic_core::unmatched::{MAX_REPORTED, UnmatchedReport};
use serde::{Deserialize, Serialize};

use crate::config::{InboundCapture, MockWebsocket, Mode, ShareConfig, StartupFallback};
use crate::state::AppState;
use crate::throttle::LogLimiter;

const TIMEOUT: Duration = Duration::from_secs(5);

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

/// Réponse d'un battement : l'état souhaité, et le sel du workload (spec §7), gardé à part
/// pour n'être ni écrit ni affiché avec l'état souhaité.
#[derive(Debug, Deserialize)]
pub struct HeartbeatReply {
    #[serde(flatten)]
    pub desired: Desired,
    #[serde(default)]
    pub redact_salt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControllerStatus {
    pub url: String,
    pub desired: Desired,
    pub last_ok_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// Taille minimale acceptée d'une réponse de battement (celle d'un battement côté
/// contrôleur, spec §9.2b).
const HEARTBEAT_REPLY: usize = 64 * 1024;

pub struct ControllerClient {
    base: String,
    instance: String,
    token_file: Option<PathBuf>,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    /// Taille maximale d'une réponse, compressée comme décompressée
    /// (`share.max_snapshot_bytes`, spec §5.6).
    max_body: usize,
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
            max_body: share.max_snapshot_bytes,
        })
    }

    /// Appel attendu en `200` ou `201`, réponse bornée à `share.max_snapshot_bytes`.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
    ) -> Result<Bytes, String> {
        self.call_within(method, path, body, self.max_body).await
    }

    /// Comme `call`, réponse bornée à `max` octets.
    async fn call_within(
        &self,
        method: Method,
        path: &str,
        body: Option<(Vec<u8>, &'static str)>,
        max: usize,
    ) -> Result<Bytes, String> {
        let r = self.send(method.clone(), path, body, None, max).await?;
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
        max: usize,
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
            Limited::new(resp.into_body(), max).collect(),
        )
        .await
        .map_err(|_| format!("{method} {path}: délai dépassé"))?
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                format!("{method} {path}: réponse au-delà de share.max_snapshot_bytes ({max} octets)")
            } else {
                format!("{method} {path}: {e}")
            }
        })?
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
        unmatched: &UnmatchedReport,
    ) -> Result<HeartbeatReply, String> {
        let body = serde_json::json!({
            "instance": self.instance,
            "mode": mode.as_str(),
            "snapshot": snapshot,
            "capture_inbound": capture.as_str(),
            "mock_websocket": websocket.as_str(),
            // Ignoré par un contrôleur d'une version antérieure (spec §9.2b).
            "unmatched": unmatched,
        });
        // Une réponse de battement n'est pas un snapshot : au moins `HEARTBEAT_REPLY`
        // octets, quelle que soit `share.max_snapshot_bytes`.
        let b = self
            .call_within(
                Method::POST,
                "/heartbeat",
                Some((body.to_string().into_bytes(), "application/json")),
                self.max_body.max(HEARTBEAT_REPLY),
            )
            .await?;
        serde_json::from_slice(&b).map_err(|e| format!("battement: {e}"))
    }

    /// Télécharge la forme de mock du snapshot (spec §5.6), compressée ; un contrôleur d'une
    /// version antérieure (`404`) ne sert que le snapshot complet, dont elle est déduite ici.
    pub async fn snapshot(&self, id: &str) -> Result<Fetched, String> {
        let path = format!("/snapshots/{id}/mock");
        let r = self
            .send(Method::GET, &path, None, Some("zstd, gzip"), self.max_body)
            .await?;
        match r.status {
            StatusCode::OK => Ok(Fetched {
                downloaded: r.body.len(),
                snapshot: mock_form(r, id, self.max_body)?,
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

/// Forme de mock reçue : décompressée (bornée à `max` octets), vérifiée contre son `ETag` et
/// l'identifiant du snapshot demandé. Le corps compressé est libéré avant la lecture.
fn mock_form(r: Reply, id: &str, max: usize) -> Result<MockSnapshot, String> {
    let encoding = r
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    let json = match encoding.as_str() {
        "identity" | "" => r.body,
        e => {
            let decoded = crate::capture::decode(e, &r.body, max).ok_or_else(|| {
                format!(
                    "forme de mock du snapshot {id}: illisible en {e}, ou au-delà de share.max_snapshot_bytes ({max} octets) une fois décompressée"
                )
            })?;
            Bytes::from(decoded)
        }
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

/// Recharge le dernier état souhaité et son snapshot depuis `store.dir` ; vrai si un état
/// a été rechargé.
pub fn restore(state: &AppState) -> bool {
    let Some(dir) = cache_dir(state) else {
        return false;
    };
    let Ok(text) = std::fs::read(dir.join("desired.json")) else {
        return false;
    };
    let Ok(desired) = serde_json::from_slice::<Desired>(&text) else {
        return false;
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
    true
}

/// Avant les écoutes (spec §5.5.5) : dernier état souhaité rechargé, sinon attente en mock
/// sur les données locales si le repli ne doit rien laisser sortir.
pub async fn prepare_startup(state: &AppState) {
    let restored = restore(state);
    let mut gate = state.startup.lock().await;
    if restored {
        gate.received();
    }
    if gate.waits_in_mock() {
        tracing::info!(
            fallback = gate.fallback().as_str(),
            "aucun état souhaité connu : attente du contrôleur en mock, rien ne sort"
        );
        state.set_mode(Mode::Mock);
    }
    state.publish_startup(&gate);
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
        return;
    }
    // L'actif est celui de l'état souhaité, pas seulement celui qui vient d'être
    // téléchargé : un `save` déclenché par un autre changement garde le même protégé.
    purge_cache(
        &dir.join("snapshots"),
        desired.snapshot.as_deref(),
        state.config.store.max_cached_snapshots,
    );
}

/// Identifiant et date de dernière écriture (millisecondes Unix) de chaque forme de mock
/// gardée sur disque.
fn cache_entries(dir: &std::path::Path) -> Vec<SnapshotEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };
    read.flatten()
        .filter_map(|entry| {
            let id = entry
                .file_name()
                .to_str()?
                .strip_suffix(".mock.json")?
                .to_string();
            let modified = entry.metadata().ok()?.modified().ok()?;
            let created_ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Some(SnapshotEntry {
                id,
                created_ms,
                pinned: false,
            })
        })
        .collect()
}

/// Purge le cache des formes de mock au-delà de `store.max_cached_snapshots` (SNP-12) : le
/// snapshot actif, s'il y en a un, n'est jamais supprimé, même le plus ancien.
fn purge_cache(dir: &std::path::Path, active: Option<&str>, max: usize) {
    let candidates = cache_entries(dir);
    let referenced: std::collections::BTreeSet<String> =
        active.map(String::from).into_iter().collect();
    for id in to_purge(&candidates, &referenced, max) {
        let _ = std::fs::remove_file(dir.join(format!("{id}.mock.json")));
    }
}

/// Au plus une ligne de journal par période pour les battements en échec (spec §9.3).
const FAILURE_LOG_PERIOD: Duration = Duration::from_secs(30);

/// Battement : annonce l'état du sidecar et applique l'état souhaité en retour.
pub fn spawn(
    state: Arc<AppState>,
    client: Arc<ControllerClient>,
    poll: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let log = LogLimiter::new(FAILURE_LOG_PERIOD);
        let mut failures = 0u64;
        loop {
            let outcome = beat(&state, &client).await;
            state
                .metrics
                .count_heartbeat(!matches!(outcome, Beat::Failed(_)), unix_secs());
            let error = match outcome {
                Beat::Aligned => None,
                Beat::Incomplete(e) | Beat::Failed(e) => Some(e),
            };
            match &error {
                None if failures > 0 => {
                    tracing::info!(failures, "battement vers le contrôleur rétabli");
                    failures = 0;
                }
                None => {}
                Some(e) => {
                    failures += 1;
                    if let Some(omitted) = log.allow(Instant::now()) {
                        tracing::warn!(error = %e, omitted, "battement vers le contrôleur échoué");
                    }
                }
            }
            {
                let mut status = state
                    .controller_status
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if error.is_none() {
                    status.last_ok_ms = Some(crate::share::now_ms());
                }
                status.last_error = error;
            }
            tokio::time::sleep(poll).await;
        }
    })
}

fn unix_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Échéance de `share.startup.timeout` (spec §5.5.5) : la barrière s'ouvre sur l'état connu,
/// sinon sur le repli ; avec `fail` sans état connu, elle reste fermée.
pub fn spawn_startup_deadline(
    state: Arc<AppState>,
    timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let mut gate = state.startup.lock().await;
        let was_open = gate.is_open();
        let fallback = gate.timeout();
        if let Some(f) = fallback {
            state.set_mode(fallback_mode(f));
        }
        state.publish_startup(&gate);
        if was_open {
            return;
        }
        let timeout_s = timeout.as_secs_f64();
        let mode = state.mode().as_str();
        match (gate.phase(), fallback) {
            (Phase::Fallback, Some(f)) => tracing::warn!(
                timeout_s,
                fallback = f.as_str(),
                mode,
                "contrôleur injoignable au démarrage : repli share.startup.fallback appliqué, l'application démarre"
            ),
            (Phase::LastKnown, _) => tracing::warn!(
                timeout_s,
                mode,
                "pas aligné à l'échéance du démarrage : dernier état souhaité connu appliqué, l'application démarre"
            ),
            _ => tracing::error!(
                timeout_s,
                "contrôleur injoignable au démarrage avec share.startup.fallback: fail : l'application attend sa réponse"
            ),
        }
    })
}

/// Mode d'un repli de démarrage.
fn fallback_mode(f: StartupFallback) -> Mode {
    match f {
        StartupFallback::Record => Mode::Record,
        StartupFallback::Passthrough => Mode::Passthrough,
        StartupFallback::MockLocal | StartupFallback::Fail => Mode::Mock,
    }
}

/// Issue d'un battement.
enum Beat {
    /// État souhaité reçu et appliqué en entier, snapshot compris.
    Aligned,
    /// État reçu mais pas appliqué en entier : snapshot non téléchargé, valeur inconnue.
    Incomplete(String),
    /// Pas de réponse exploitable du contrôleur.
    Failed(String),
}

/// État souhaité traduit ; un champ absent prend la valeur de la configuration (ressource
/// supprimée, réglage plus imposé).
struct Wanted {
    mode: Mode,
    capture: InboundCapture,
    websocket: MockWebsocket,
}

impl Wanted {
    fn parse(state: &AppState, d: &Desired) -> Result<Self, String> {
        let mode = match d.mode.as_deref() {
            Some(m) => Mode::parse(m).ok_or_else(|| format!("mode souhaité inconnu: {m}"))?,
            None => state.config.mode,
        };
        let capture = match d.capture_inbound.as_deref() {
            Some(c) => InboundCapture::parse(c)
                .ok_or_else(|| format!("capture de l'entrant inconnue: {c}"))?,
            None => state.config.capture.inbound,
        };
        let websocket = match d.mock_websocket.as_deref() {
            Some(w) => {
                MockWebsocket::parse(w).ok_or_else(|| format!("réglage WebSocket inconnu: {w}"))?
            }
            None => state.config.mock.websocket,
        };
        Ok(Self {
            mode,
            capture,
            websocket,
        })
    }

    /// Le snapshot d'abord (`Some`, son retrait compris) : un passage en mock sert ainsi
    /// directement le bon jeu.
    fn apply(&self, state: &AppState, snapshot: Option<Option<Arc<MockSnapshot>>>) {
        if let Some(s) = snapshot {
            state.set_snapshot(s);
        }
        state.set_mode(self.mode);
        state.set_inbound_capture(self.capture);
        state.set_mock_websocket(self.websocket);
    }
}

async fn beat(state: &AppState, client: &ControllerClient) -> Beat {
    let loaded = state.snapshot().map(|s| s.id.clone());
    let desired = match client
        .heartbeat(
            state.mode(),
            loaded.clone(),
            state.inbound_capture(),
            state.mock_websocket(),
            &state.unmatched_report(MAX_REPORTED),
        )
        .await
    {
        Ok(reply) => {
            if let Some(salt) = &reply.redact_salt {
                state.set_workload_salt(salt);
            }
            reply.desired
        }
        Err(e) => return Beat::Failed(e),
    };
    let wanted = match Wanted::parse(state, &desired) {
        Ok(w) => w,
        Err(e) => return Beat::Incomplete(e),
    };
    let previous = state
        .controller_status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .desired
        .clone();
    let fetch = desired
        .snapshot
        .as_ref()
        .filter(|id| loaded.as_ref() != Some(*id));
    let unsaved = {
        let mut gate = state.startup.lock().await;
        // Un état connu avant ce battement est déjà sur disque (rechargé, ou reçu et écrit).
        let unsaved = !gate.known();
        if fetch.is_some() && !gate.is_open() {
            // Barrière fermée : l'application n'a pas démarré, l'état reçu s'applique dès la
            // réponse, en mock sur les données locales le temps du téléchargement (§5.5.5).
            wanted.apply(state, None);
            gate.received();
            state.publish_startup(&gate);
        }
        unsaved
    };
    // S'il ne peut pas être téléchargé, le mode est appliqué quand même (le mock sert alors
    // les données locales, rien ne sort) et le téléchargement est retenté au battement suivant.
    let mut fetched = None;
    let mut fetch_error = None;
    if let Some(id) = fetch {
        match client.snapshot(id).await {
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
                fetched = Some(snap);
            }
            Err(e) => fetch_error = Some(format!("snapshot {id}: {e}")),
        }
    }
    let snapshot = match &fetched {
        Some(s) => Some(Some(s.clone())),
        None if desired.snapshot.is_none() && loaded.is_some() => Some(None),
        None => None,
    };
    {
        let mut gate = state.startup.lock().await;
        wanted.apply(state, snapshot);
        if fetch_error.is_none() {
            gate.aligned();
        } else {
            gate.received();
        }
        state.publish_startup(&gate);
    }
    if desired != previous || fetched.is_some() || unsaved {
        save(state, &desired, fetched.as_deref());
        state
            .controller_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .desired = desired;
    }
    fetch_error.map_or(Beat::Aligned, Beat::Incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn touch(dir: &std::path::Path, id: &str, age: Duration) {
        let path = dir.join(format!("{id}.mock.json"));
        fs::write(&path, b"{}").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    /// SNP-12 : au-delà de `max`, les plus anciennes formes de mock sont supprimées,
    /// jamais celle du snapshot actif, même la plus ancienne.
    #[test]
    fn snp_12_purge_keeps_the_active_and_the_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "oldest", Duration::from_secs(40));
        touch(dir.path(), "middle", Duration::from_secs(30));
        touch(dir.path(), "recent", Duration::from_secs(10));
        touch(dir.path(), "newest", Duration::from_secs(0));

        purge_cache(dir.path(), Some("oldest"), 2);

        let remaining: std::collections::BTreeSet<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".mock.json")
                    .map(String::from)
            })
            .collect();
        assert_eq!(
            remaining,
            ["oldest", "recent", "newest"]
                .into_iter()
                .map(String::from)
                .collect(),
            "{remaining:?}"
        );
    }

    /// Sans snapshot actif (pod hors mock), seules les `max` plus récentes survivent.
    #[test]
    fn snp_12_purge_without_an_active_snapshot_keeps_only_the_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a", Duration::from_secs(20));
        touch(dir.path(), "b", Duration::from_secs(10));
        touch(dir.path(), "c", Duration::from_secs(0));

        purge_cache(dir.path(), None, 1);

        let remaining: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(remaining.len(), 1, "{remaining:?}");
        assert!(dir.path().join("c.mock.json").exists());
    }
}
