//! Bascule de mode par annotation de pod, lue dans le fichier de l'API downward (spec §5.1).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Mode;
use crate::state::AppState;

pub const MODE_ANNOTATION: &str = "mimic.platform.io/mode";
const POLL: Duration = Duration::from_secs(1);

/// Valeur d'une annotation dans le format du kubelet : `clé="valeur"` par ligne.
pub fn annotation(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let v = v.trim();
        // Les valeurs sont des chaînes entre guillemets, échappées comme du JSON.
        serde_json::from_str::<String>(v)
            .ok()
            .or_else(|| Some(v.trim_matches('"').to_string()))
    })
}

/// Applique le mode de l'annotation à chaque changement de sa valeur. Une bascule faite
/// via l'API admin reste en place tant que l'annotation ne change pas.
pub async fn watch_annotations(state: Arc<AppState>, path: PathBuf) {
    let mut last: Option<String> = None;
    loop {
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            let value = annotation(&content, MODE_ANNOTATION);
            if value != last {
                if let Some(v) = &value {
                    match Mode::parse(v) {
                        Some(m) => state.set_mode(m),
                        None => tracing::warn!(value = %v, "annotation de mode invalide"),
                    }
                }
                last = value;
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kubelet_format() {
        let c = "a=\"1\"\nmimic.platform.io/mode=\"mock\"\nb=\"x\\\"y\"\n";
        assert_eq!(annotation(c, MODE_ANNOTATION).as_deref(), Some("mock"));
        assert_eq!(annotation(c, "b").as_deref(), Some("x\"y"));
        assert_eq!(annotation(c, "absent"), None);
    }
}
