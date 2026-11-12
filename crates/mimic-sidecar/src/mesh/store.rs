//! The persisted view, `<MIMIC_TRUST_DIR>/state/mesh.json` (spec §7.9): written through a
//! temporary file and a rename, read back bounded and validated like a pulled view.

use std::io::Read;
use std::path::Path;

use mimic_core::mesh::view::{Limits, View, etag};
use serde::Deserialize;

/// What the file holds besides the view.
const SLACK: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "fetchedAt")]
    fetched_at: u64,
    view: serde_json::Value,
}

/// `fetchedAt`, the view and its `ETag`; `None` without a file.
pub(super) fn read(path: &Path, limits: &Limits) -> Result<Option<(u64, View, String)>, String> {
    // A FIFO or a device would block the open or the read: only a regular file is read.
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("not a regular file".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read: {}", e.kind())),
    }
    let cap = limits.max_view_bytes + SLACK;
    let file = std::fs::File::open(path).map_err(|e| format!("cannot read: {}", e.kind()))?;
    let mut text = String::new();
    file.take(cap as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read: {}", e.kind()))?;
    if text.len() > cap {
        return Err("too large".to_string());
    }
    let file: File =
        serde_json::from_str(&text).map_err(|_| "not a mesh state file".to_string())?;
    let json = file.view.to_string();
    let view = View::from_json(&json, limits)?;
    // The canonical bytes of the view are what its `ETag` names.
    let etag = etag(&view.to_json());
    Ok(Some((file.fetched_at, view, etag)))
}

pub(super) fn write(path: &Path, fetched_at: u64, view_json: &str) -> Result<(), String> {
    let error = |what: &str, e: std::io::Error| format!("{what}: {}", e.kind());
    let dir = path.parent().ok_or("no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| error("create directory", e))?;
    let text = format!("{{\"fetchedAt\":{fetched_at},\"view\":{view_json}}}");
    let temporary = dir.join("mesh.tmp");
    std::fs::write(&temporary, text).map_err(|e| error("write", e))?;
    std::fs::rename(&temporary, path).map_err(|e| error("rename", e))
}
