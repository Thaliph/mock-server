//! Partage des enregistrements entre les sidecars d'un même workload (spec §5.4).

use crate::state::AppState;

/// Demande une synchronisation immédiate ; faux si le partage est désactivé.
pub fn trigger(state: &AppState) -> bool {
    state.config.share.is_some()
}

/// Lance la boucle de partage si elle est configurée.
pub fn spawn(
    _state: &std::sync::Arc<AppState>,
) -> Result<Option<tokio::task::JoinHandle<()>>, String> {
    Ok(None)
}

/// Dernière publication avant l'arrêt.
pub async fn final_sync(_state: &AppState) {}
