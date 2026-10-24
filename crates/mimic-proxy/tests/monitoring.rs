//! Critères MON-01 et MON-05 de la spec (§9.3), côté sidecar.

mod common;

use std::path::Path;

use common::*;
use mimic_proxy::config::{Config, Mode};

#[tokio::test]
async fn mon_01_sidecar_exposes_build_info() {
    let p = proxy(config(None, Mode::Record)).await;
    let m = metrics(&p).await;
    let prefix = format!(
        "mimic_build_info{{mimic_version=\"{}\",revision=\"",
        env!("CARGO_PKG_VERSION")
    );
    let line = m
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{prefix} absente :\n{m}"));
    assert!(line.ends_with("\"} 1"), "{line}");
}

/// Somme des tailles des fichiers sous `dir`, sous-répertoires compris.
fn disk_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| {
            let meta = e.metadata().unwrap();
            if meta.is_dir() {
                disk_bytes(&e.path())
            } else {
                meta.len()
            }
        })
        .sum()
}

#[tokio::test]
async fn mon_05_store_disk_usage_and_limit_are_exposed() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(None, Mode::Record);
    c.store.dir = Some(dir.path().to_path_buf());
    c.store.size_limit_bytes = Some(1_048_576);
    let p = proxy(c).await;

    let m = metrics(&p).await;
    assert_eq!(
        metric_value(&m, "mimic_store_disk_limit_bytes"),
        Some(1_048_576.0)
    );
    assert_eq!(
        metric_value(&m, "mimic_store_disk_bytes"),
        Some(disk_bytes(dir.path()) as f64),
        "{m}"
    );

    // Une forme de mock gardée compte, dans son sous-répertoire.
    let snapshots = dir.path().join("controller/snapshots");
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::write(snapshots.join("s1.mock.json"), vec![b'x'; 5000]).unwrap();
    let m = metrics(&p).await;
    let used = metric_value(&m, "mimic_store_disk_bytes").unwrap();
    assert!(used >= 5000.0, "{m}");
    assert_eq!(used, disk_bytes(dir.path()) as f64);
}

#[tokio::test]
async fn mon_05_without_store_dir_nothing_is_used_and_the_limit_is_unknown() {
    let p = proxy(config(None, Mode::Record)).await;
    let m = metrics(&p).await;
    assert_eq!(metric_value(&m, "mimic_store_disk_bytes"), Some(0.0));
    assert_eq!(metric_value(&m, "mimic_store_disk_limit_bytes"), Some(0.0));
}

#[test]
fn mon_05_size_limit_comes_from_the_environment() {
    let mut c: Config = serde_yaml::from_str("store: { size_limit_bytes: 1024 }").unwrap();
    assert_eq!(c.store.size_limit_bytes, Some(1024));
    c.apply_env(|k| (k == "MIMIC_STORE_SIZE_LIMIT_BYTES").then(|| "536870912".into()))
        .unwrap();
    assert_eq!(c.store.size_limit_bytes, Some(536_870_912));
    let err = c
        .apply_env(|k| (k == "MIMIC_STORE_SIZE_LIMIT_BYTES").then(|| "512Mi".into()))
        .unwrap_err();
    assert!(err.contains("MIMIC_STORE_SIZE_LIMIT_BYTES"), "{err}");
}
