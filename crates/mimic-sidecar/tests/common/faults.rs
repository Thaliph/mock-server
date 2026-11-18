//! The faults of a view and the checks of their answers, shared by the fault injection tests
//! (spec §7.9 "Fault injection", §12.28).
#![allow(dead_code)]

use super::{Fixture, metric};

/// A fault matching `Exact path`, aborting `percent` % with `status`.
pub fn abort_on(path: &str, percent: u32, status: u16) -> String {
    format!(
        r#"{{"abort":{{"percent":{percent},"status":{status}}},"match":{{"path":{{"type":"Exact","value":"{path}"}}}}}}"#
    )
}

/// A fault without `match` delaying `percent` % by `ms`.
pub fn delay(percent: u32, ms: u64) -> String {
    format!(r#"[{{"delay":{{"fixedMs":{ms},"percent":{percent}}}}}]"#)
}

/// The status of a response head.
pub fn status(head: &str) -> u16 {
    head[9..12].parse().unwrap()
}

/// Whether the access line has no such field.
pub fn absent(line: &serde_json::Value, field: &str) -> bool {
    line.get(field).is_none_or(serde_json::Value::is_null)
}

/// A response head lower-cased.
pub fn lower(head: &str) -> String {
    head.to_ascii_lowercase()
}

/// The count of the faults of `kind` of `shop/cart` in the scrape of `f`.
pub fn faults(f: &Fixture, kind: &str) -> f64 {
    metric(
        &f.metrics.encode(),
        "mimic_mesh_faults_total{",
        &["service=\"shop/cart\"", &format!("kind=\"{kind}\"")],
    )
}

/// Whether the scrape has no series of `name`.
pub fn no_series(scrape: &str, name: &str) -> bool {
    !scrape.lines().any(|l| l.starts_with(name))
}
