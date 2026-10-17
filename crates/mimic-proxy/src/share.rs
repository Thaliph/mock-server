//! Partage des enregistrements entre les sidecars d'un même workload (spec §5.4).
//!
//! Chaque sidecar publie ses nouveaux échanges (déjà redactés) dans un segment JSONL sous
//! `<prefixe>/<namespace>/<workload>/exchanges/<instance>/`, puis importe les segments des
//! autres instances. Aucun appel à l'API Kubernetes, aucune connexion entre pods.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use mimic_core::model::Exchange;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::config::ShareConfig;
use crate::state::AppState;
use crate::store::Origin;

/// Demande une synchronisation immédiate ; faux si le partage est désactivé.
pub fn trigger(state: &AppState) -> bool {
    if state.config.share.is_none() {
        return false;
    }
    state.share_notify.notify_one();
    true
}

/// Lance la boucle de partage si elle est configurée.
pub fn spawn(state: &Arc<AppState>) -> Result<Option<tokio::task::JoinHandle<()>>, String> {
    let Some(config) = state.config.share.clone() else {
        return Ok(None);
    };
    let base = base_path(state);
    let state = state.clone();
    Ok(Some(tokio::spawn(async move {
        let mut sharer = Sharer::new(config, base);
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
pub async fn final_sync(state: &AppState) {
    let Some(config) = state.config.share.clone() else {
        return;
    };
    let mut sharer = Sharer::new(config, base_path(state));
    let publish = async {
        if let Ok(store) = sharer.backend() {
            let _ = publish(state, store.as_ref(), &sharer.base).await;
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
    store: Option<Arc<dyn ObjectStore>>,
    imported: HashSet<String>,
}

impl Sharer {
    fn new(config: ShareConfig, base: String) -> Self {
        Self {
            config,
            base,
            store: None,
            imported: HashSet::new(),
        }
    }

    /// Construit le support à la demande : une erreur au démarrage est retentée au cycle suivant.
    fn backend(&mut self) -> Result<Arc<dyn ObjectStore>, String> {
        if let Some(s) = &self.store {
            return Ok(s.clone());
        }
        let store = open(&self.config.url)?;
        self.store = Some(store.clone());
        Ok(store)
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
        let store = self.backend()?;
        publish(state, store.as_ref(), &self.base).await?;

        let me = &state.config.instance;
        let oldest_ms = now_ms().saturating_sub(self.config.retention.as_millis() as u64) as i64;
        let prefix = Path::from(self.base.as_str());
        let mut listing = store.list(Some(&prefix));
        let mut peers = BTreeSet::new();
        let mut to_import = vec![];
        while let Some(meta) = listing.next().await {
            let meta = meta.map_err(|e| format!("liste du partage: {e}"))?;
            let location = meta.location.to_string();
            let Some(instance) = instance_of(&self.base, &location) else {
                continue;
            };
            if instance == *me {
                continue;
            }
            peers.insert(instance.to_string());
            if meta.last_modified.timestamp_millis() < oldest_ms
                || self.imported.contains(&location)
            {
                continue;
            }
            to_import.push(meta.location);
        }
        let mut imported = 0;
        for location in to_import {
            let bytes = store
                .get(&location)
                .await
                .map_err(|e| format!("lecture de {location}: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("lecture de {location}: {e}"))?;
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if let Ok(e) = serde_json::from_slice::<Exchange>(line) {
                    state.store.add(e, Origin::Peer);
                    imported += 1;
                }
            }
            self.imported.insert(location.to_string());
        }
        if imported > 0 {
            tracing::debug!(imported, "échanges importés des autres sidecars");
            state.refresh_store_metrics();
        }
        Ok(peers)
    }
}

/// Publie les nouveaux échanges locaux ; en cas d'échec ils restent en attente.
async fn publish(state: &AppState, store: &dyn ObjectStore, base: &str) -> Result<(), String> {
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
    let path = Path::from(format!(
        "{base}/{}/{}.jsonl",
        state.config.instance,
        ulid::Ulid::generate()
    ));
    match store.put(&path, PutPayload::from(body)).await {
        Ok(_) => Ok(()),
        Err(e) => {
            state.store.restore_unshared(pending);
            Err(format!("publication de {path}: {e}"))
        }
    }
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

fn now_ms() -> u64 {
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
}
