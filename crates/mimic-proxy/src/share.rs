//! Partage des enregistrements entre les sidecars d'un même workload (spec §5.4).
//!
//! Chaque sidecar publie ses nouveaux échanges (déjà redactés) dans des segments JSONL, puis
//! importe les segments des autres instances. Le support est le contrôleur (recommandé) ou un
//! stockage objet partagé sous `<prefixe>/<namespace>/<workload>/exchanges/<instance>/`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mimic_core::model::Exchange;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::config::ShareConfig;
use crate::control::ControllerClient;
use crate::state::AppState;
use crate::store::Origin;

/// Un segment publié ne dépasse pas cette taille (le contrôleur refuse au-delà de 64 MiB).
const MAX_SEGMENT_BYTES: usize = 8 * 1024 * 1024;
/// Marge de relecture de la liste incrémentale : couvre les écritures concurrentes.
const LIST_OVERLAP_MS: i64 = 60_000;
/// Attente maximale entre deux synchronisations après des échecs.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Support du partage.
#[derive(Clone)]
enum Backend {
    Object(Arc<dyn ObjectStore>),
    Controller(Arc<ControllerClient>),
}

/// Segment vu sur le support : clé de lecture, instance qui l'a écrit, date.
struct Seen {
    key: String,
    instance: String,
    modified_ms: i64,
}

/// Premier segment de `list` : lignes JSONL d'au plus `max_bytes` (un échange plus gros
/// forme son propre segment), avec le nombre d'échanges qu'il couvre. Une ligne qui ne se
/// sérialise pas compte sans être écrite.
fn first_segment(list: &[Arc<Exchange>], max_bytes: usize) -> (Vec<u8>, usize) {
    let mut body = Vec::new();
    let mut count = 0;
    for e in list {
        let Ok(mut line) = serde_json::to_vec(e.as_ref()) else {
            count += 1;
            continue;
        };
        line.push(b'\n');
        if count > 0 && body.len() + line.len() > max_bytes {
            break;
        }
        body.extend_from_slice(&line);
        count += 1;
    }
    (body, count)
}

/// Lignes JSONL d'un segment scellé.
fn lines(list: &[Arc<Exchange>]) -> Vec<u8> {
    first_segment(list, usize::MAX).0
}

/// Demande une synchronisation immédiate ; faux si le partage est désactivé.
pub fn trigger(state: &AppState) -> bool {
    if state.config.share.is_none() {
        return false;
    }
    state.share_notify.notify_one();
    true
}

/// Lance la boucle de partage si elle est configurée.
pub fn spawn(
    state: &Arc<AppState>,
    controller: Option<Arc<ControllerClient>>,
) -> Result<Option<tokio::task::JoinHandle<()>>, String> {
    let Some(config) = state.config.share.clone() else {
        return Ok(None);
    };
    let base = base_path(state);
    let state = state.clone();
    Ok(Some(tokio::spawn(async move {
        let mut sharer = Sharer::new(config, base, controller);
        let mut failures = 0;
        loop {
            failures = if sharer.sync(&state).await {
                0
            } else {
                failures + 1
            };
            let wait = backoff(sharer.config.interval, failures);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = state.share_notify.notified() => {}
            }
        }
    })))
}

/// Attente avant la prochaine synchronisation : `interval`, puis `interval` × 2ⁿ après n
/// échecs consécutifs, plafonnée, avec une gigue de 50 % pour étaler les reprises.
fn backoff(interval: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return interval;
    }
    let max = MAX_BACKOFF.max(interval);
    let wait = interval.saturating_mul(1 << failures.min(16)).min(max);
    let jitter = (ulid::Ulid::generate().random() % 1_000) as f64 / 2_000.0;
    wait.mul_f64(0.5 + jitter)
}

/// Dernière publication avant l'arrêt, bornée dans le temps.
pub async fn final_sync(state: &AppState, controller: Option<Arc<ControllerClient>>) {
    let Some(config) = state.config.share.clone() else {
        return;
    };
    let mut sharer = Sharer::new(config, base_path(state), controller);
    let publish = async {
        if let Ok(backend) = sharer.backend() {
            let _ = publish(state, &backend, &sharer.base).await;
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), publish).await;
}

fn base_path(state: &AppState) -> String {
    let w = &state.config.workload;
    let ns = if w.namespace.is_empty() {
        "default"
    } else {
        &w.namespace
    };
    format!("{ns}/{}/exchanges", w.name)
}

struct Sharer {
    config: ShareConfig,
    /// Chemin relatif au support, sans le préfixe de l'URL.
    base: String,
    backend: Option<Backend>,
    /// Segments importés, avec leur date : ceux d'avant la fenêtre de la liste incrémentale
    /// ne seront plus listés et sont oubliés.
    imported: BTreeMap<String, i64>,
    /// Date (ms) du segment le plus récent déjà listé ; n'avance qu'après un import réussi.
    listed_upto_ms: Option<i64>,
    /// Instances vues dans les listes successives.
    peers: BTreeSet<String>,
}

impl Sharer {
    fn new(config: ShareConfig, base: String, controller: Option<Arc<ControllerClient>>) -> Self {
        Self {
            config,
            base,
            backend: controller.map(Backend::Controller),
            imported: BTreeMap::new(),
            listed_upto_ms: None,
            peers: BTreeSet::new(),
        }
    }

    /// Construit le support à la demande : une erreur au démarrage est retentée au cycle suivant.
    fn backend(&mut self) -> Result<Backend, String> {
        if let Some(b) = &self.backend {
            return Ok(b.clone());
        }
        let b = Backend::Object(open(&self.config.url)?);
        self.backend = Some(b.clone());
        Ok(b)
    }

    /// Une synchronisation ; vrai si elle a réussi.
    async fn sync(&mut self, state: &AppState) -> bool {
        let result = self.try_sync(state).await;
        let ok = result.is_ok();
        state
            .metrics
            .share_sync
            .get_or_create(&crate::metrics::ResultLabel {
                result: if ok { "ok" } else { "error" }.into(),
            })
            .inc();
        let mut status = state.share_status.lock().expect("verrou du partage");
        match result {
            Ok(peers) => {
                state.metrics.share_peers.set(peers.len() as i64);
                status.peers = peers.into_iter().collect();
                status.last_sync_ms = Some(now_ms());
                status.last_error = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "synchronisation du partage échouée");
                status.last_error = Some(e);
            }
        }
        ok
    }

    async fn try_sync(&mut self, state: &AppState) -> Result<BTreeSet<String>, String> {
        let backend = self.backend()?;
        publish(state, &backend, &self.base).await?;

        let me = &state.config.instance;
        let oldest_ms = now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64;
        let since = self.listed_upto_ms.map(|ms| ms - LIST_OVERLAP_MS);
        let listed = list(&backend, &self.base, since).await?;
        let newest = listed.iter().map(|s| s.modified_ms).max();
        let mut to_import = vec![];
        for seg in listed {
            // Filtre local : un stockage objet, ou un ancien contrôleur, liste tout.
            if seg.instance == *me || since.is_some_and(|s| seg.modified_ms < s) {
                continue;
            }
            // Instances réservées (`_hour`, segments horaires du contrôleur) : pas des pairs.
            if !seg.instance.starts_with('_') {
                self.peers.insert(seg.instance.clone());
            }
            if seg.modified_ms < oldest_ms || self.imported.contains_key(&seg.key) {
                continue;
            }
            to_import.push((seg.key, seg.modified_ms));
        }
        let mut imported = 0;
        for (key, at) in to_import {
            let bytes = get(&backend, &key).await?;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(e) = serde_json::from_slice::<Exchange>(line) {
                    state.store.add(e, Origin::Peer);
                    imported += 1;
                }
            }
            self.imported.insert(key, at);
        }
        // Tout est importé : le curseur avance, la fenêtre suivante commence plus tard.
        if let Some(max) = newest {
            let upto = self.listed_upto_ms.map_or(max, |m| m.max(max));
            self.listed_upto_ms = Some(upto);
            self.imported.retain(|_, at| *at >= upto - LIST_OVERLAP_MS);
        }
        if imported > 0 {
            tracing::debug!(imported, "échanges importés des autres sidecars");
            state.refresh_store_metrics();
        }
        Ok(self.peers.clone())
    }
}

async fn list(backend: &Backend, base: &str, since_ms: Option<i64>) -> Result<Vec<Seen>, String> {
    match backend {
        Backend::Controller(c) => Ok(c
            .segments(since_ms)
            .await?
            .into_iter()
            .map(|s| Seen {
                key: s.key,
                instance: s.instance,
                modified_ms: s.modified_ms,
            })
            .collect()),
        Backend::Object(store) => {
            let prefix = Path::from(base);
            let mut listing = store.list(Some(&prefix));
            let mut out = vec![];
            while let Some(meta) = listing.next().await {
                let meta = meta.map_err(|e| format!("liste du partage: {e}"))?;
                let key = meta.location.to_string();
                let Some(instance) = instance_of(base, &key) else {
                    continue;
                };
                out.push(Seen {
                    instance: instance.to_string(),
                    modified_ms: meta.last_modified.timestamp_millis(),
                    key,
                });
            }
            Ok(out)
        }
    }
}

async fn get(backend: &Backend, key: &str) -> Result<bytes::Bytes, String> {
    match backend {
        Backend::Controller(c) => c.segment(key).await,
        Backend::Object(store) => store
            .get(&Path::from(key))
            .await
            .map_err(|e| format!("lecture de {key}: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("lecture de {key}: {e}")),
    }
}

/// Prochain segment à publier : celui déjà scellé (renvoyé sous le même identifiant), sinon
/// un nouveau, formé en tête de la file du store (spec §5.4).
fn next_segment(state: &AppState) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some((id, list)) = state.store.sealed_unshared() {
            return Some((id, lines(&list)));
        }
        let head = state.store.unshared_head(MAX_SEGMENT_BYTES);
        let first = head.first()?.id.clone();
        // Sérialisation hors du verrou du store ; si la tête a bougé entre-temps (échanges
        // jetés par la borne), on recommence.
        let (body, count) = first_segment(&head, MAX_SEGMENT_BYTES);
        if let Some(id) = state.store.seal_unshared(&first, count) {
            return Some((id, body));
        }
    }
}

/// Publie la file, du plus ancien au plus récent. Un segment refusé reste scellé en tête de
/// la file, avec son identifiant, et arrête la publication.
async fn publish(state: &AppState, backend: &Backend, base: &str) -> Result<(), String> {
    while let Some((id, body)) = next_segment(state) {
        match backend {
            Backend::Controller(c) => c.put_segment(&id, body).await?,
            Backend::Object(store) => {
                let path = Path::from(format!("{base}/{}/{id}.jsonl", state.config.instance));
                store
                    .put(&path, PutPayload::from(body))
                    .await
                    .map_err(|e| format!("publication de {path}: {e}"))?;
            }
        }
        state.store.commit_unshared(&id);
    }
    Ok(())
}

/// Instance qui a écrit un segment : premier segment du chemin après la base.
fn instance_of<'a>(base: &str, location: &'a str) -> Option<&'a str> {
    let rest = location.strip_prefix(base)?.strip_prefix('/')?;
    let (instance, file) = rest.split_once('/')?;
    file.ends_with(".jsonl").then_some(instance)
}

fn open(url: &str) -> Result<Arc<dyn ObjectStore>, String> {
    if let Some(dir) = url.strip_prefix("file://") {
        std::fs::create_dir_all(dir).map_err(|e| format!("répertoire de partage {dir}: {e}"))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(dir)
            .map_err(|e| format!("répertoire de partage {dir}: {e}"))?;
        return Ok(Arc::new(fs));
    }
    if let Some(rest) = url.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        // Identifiants, région et endpoint lus dans l'environnement (AWS_*), IRSA compris.
        let s3 = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .map_err(|e| format!("bucket {bucket}: {e}"))?;
        let store: Arc<dyn ObjectStore> = Arc::new(s3);
        return Ok(if prefix.trim_matches('/').is_empty() {
            store
        } else {
            Arc::new(object_store::prefix::PrefixStore::new(
                store,
                prefix.trim_matches('/'),
            ))
        });
    }
    Err(format!("URL de partage non supportée: {url}"))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mimic_core::model::{Body, Direction};

    fn ex(id: &str, body: usize) -> Exchange {
        Exchange {
            id: id.into(),
            direction: Direction::Outbound,
            started_at_ms: 0,
            scheme: "http".into(),
            authority: "a:80".into(),
            method: "GET".into(),
            path: "/".into(),
            query: vec![],
            req_headers: vec![],
            req_body: Body::Empty,
            status: 200,
            resp_headers: vec![],
            resp_body: Body::captured(vec![b'x'; body]),
            ttfb_us: 1,
            total_us: 1,
            trace_id: None,
            redactions: vec![],
        }
    }

    fn ids(body: &[u8]) -> Vec<String> {
        body.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice::<Exchange>(l).unwrap().id)
            .collect()
    }

    #[test]
    fn instances_from_paths() {
        let base = "ns/api/exchanges";
        assert_eq!(
            instance_of(base, "ns/api/exchanges/api-a/01H.jsonl"),
            Some("api-a")
        );
        assert_eq!(instance_of(base, "ns/api/exchanges/api-a"), None);
        assert_eq!(instance_of(base, "ns/apix/exchanges/a/1.jsonl"), None);
    }

    #[test]
    fn shr_09_segments_are_split_at_the_size_limit() {
        let list: Vec<Arc<Exchange>> = (0..5).map(|i| Arc::new(ex(&i.to_string(), 300))).collect();
        let line = serde_json::to_vec(list[0].as_ref()).unwrap().len() + 1;
        let (body, count) = first_segment(&list, 2 * line);
        assert_eq!(count, 2);
        assert_eq!(ids(&body), ["0", "1"]);
        assert!(body.len() <= 2 * line);
        // Un échange plus gros que la limite forme son propre segment.
        let big = [Arc::new(ex("big", 4 * line)), Arc::new(ex("small", 1))];
        assert_eq!(first_segment(&big, line).1, 1);
        assert_eq!(ids(&lines(&list)).len(), 5);
    }

    #[test]
    fn shr_08_cursor_advances_only_after_a_complete_import() {
        let t = 10_000_000;
        let mut c = Cursor::default();
        assert_eq!(c.since(), None);
        assert!(c.wants("p/1", t, 0));
        // Hors rétention : jamais importé.
        assert!(!c.wants("p/0", t, t + 1));
        // Import échoué : rien n'est marqué, le curseur ne bouge pas.
        assert_eq!(c.since(), None);
        assert!(c.wants("p/1", t, 0));
        // Import complet : le segment n'est plus voulu, la fenêtre suit le plus récent.
        c.imported("p/1", t);
        c.complete(Some(t));
        assert_eq!(c.since(), Some(t - LIST_OVERLAP_MS));
        assert!(!c.wants("p/1", t, 0));
        assert!(!c.wants("p/old", t - LIST_OVERLAP_MS - 1, 0));
        assert!(c.wants("p/2", t - LIST_OVERLAP_MS, 0));
        // Les segments sortis de la fenêtre sont oubliés.
        c.imported("p/2", t - LIST_OVERLAP_MS);
        c.complete(Some(t + 2 * LIST_OVERLAP_MS));
        assert_eq!(c.remembered(), 0);
    }

    #[test]
    fn shr_09_backoff_grows_and_is_capped() {
        let i = Duration::from_secs(10);
        assert_eq!(backoff(i, 0), i);
        for n in 1..8 {
            let w = backoff(i, n);
            let full = (i * (1 << n)).min(MAX_BACKOFF);
            assert!(w >= full / 2 && w <= full, "{n} : {w:?}");
        }
        assert!(backoff(i, 40) <= MAX_BACKOFF);
    }

    /// Deux fournisseurs rustls sont compilés (aws-lc-rs vient d'object_store) : le
    /// client S3 ne doit pas paniquer faute d'en avoir choisi un.
    #[tokio::test]
    async fn s3_store_does_not_panic_on_crypto_provider() {
        let store = open("s3://mimic-test/prefixe").unwrap();
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures_util::StreamExt::next(&mut store.list(None)),
        )
        .await;
        // Réseau absent ou bucket inconnu : une erreur, jamais une panique.
        if let Ok(Some(item)) = first {
            assert!(item.is_err());
        }
    }
}
