//! Partage des enregistrements entre les sidecars d'un même workload (spec §5.4).
//!
//! Chaque sidecar publie ses nouveaux échanges (déjà redactés) dans un segment JSONL, puis
//! importe les segments des autres instances. Le support est le contrôleur (recommandé) ou un
//! stockage objet partagé sous `<prefixe>/<namespace>/<workload>/exchanges/<instance>/`.

use std::collections::{BTreeSet, HashSet};
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
        loop {
            sharer.sync(&state).await;
            let interval = sharer.config.interval;
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = state.share_notify.notified() => {}
            }
        }
    })))
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
    imported: HashSet<String>,
}

impl Sharer {
    fn new(config: ShareConfig, base: String, controller: Option<Arc<ControllerClient>>) -> Self {
        Self {
            config,
            base,
            backend: controller.map(Backend::Controller),
            imported: HashSet::new(),
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

    async fn sync(&mut self, state: &AppState) {
        let result = self.try_sync(state).await;
        let label = if result.is_ok() { "ok" } else { "error" };
        state
            .metrics
            .share_sync
            .get_or_create(&crate::metrics::ResultLabel {
                result: label.into(),
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
    }

    async fn try_sync(&mut self, state: &AppState) -> Result<BTreeSet<String>, String> {
        let backend = self.backend()?;
        publish(state, &backend, &self.base).await?;

        let me = &state.config.instance;
        let oldest_ms = now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64;
        let mut peers = BTreeSet::new();
        let mut to_import = vec![];
        for seg in list(&backend, &self.base).await? {
            if seg.instance == *me {
                continue;
            }
            peers.insert(seg.instance.clone());
            if seg.modified_ms < oldest_ms || self.imported.contains(&seg.key) {
                continue;
            }
            to_import.push(seg.key);
        }
        let mut imported = 0;
        for key in to_import {
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
            self.imported.insert(key);
        }
        if imported > 0 {
            tracing::debug!(imported, "échanges importés des autres sidecars");
            state.refresh_store_metrics();
        }
        Ok(peers)
    }
}

async fn list(backend: &Backend, base: &str) -> Result<Vec<Seen>, String> {
    match backend {
        Backend::Controller(c) => Ok(c
            .segments()
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

/// Publie les nouveaux échanges locaux ; en cas d'échec ils restent en attente.
async fn publish(state: &AppState, backend: &Backend, base: &str) -> Result<(), String> {
    let pending = state.store.take_unshared();
    if pending.is_empty() {
        return Ok(());
    }
    let mut body = Vec::new();
    for e in &pending {
        if let Ok(line) = serde_json::to_vec(e) {
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
    }
    let result = match backend {
        Backend::Controller(c) => c.put_segment(body).await,
        Backend::Object(store) => {
            let path = Path::from(format!(
                "{base}/{}/{}.jsonl",
                state.config.instance,
                ulid::Ulid::generate()
            ));
            store
                .put(&path, PutPayload::from(body))
                .await
                .map(|_| ())
                .map_err(|e| format!("publication de {path}: {e}"))
        }
    };
    if result.is_err() {
        state.store.restore_unshared(pending);
    }
    result
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
