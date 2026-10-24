//! Critère PRX-11 (spec §9.3) : échecs vers l'amont tous comptés, journalisés au plus une
//! fois par seconde.
//!
//! Seul test de ce binaire : le journal est capturé par un abonné propre au fil du test, et
//! un autre test qui échouerait vers l'amont en parallèle, sans abonné, fausserait le cache
//! d'intérêt du site de log de `tracing` (aucune ligne capturée).

mod common;

use std::time::{Duration, Instant};

use common::*;
use mimic_proxy::config::Mode;

/// Journal de test : les lignes de log émises sur ce fil (runtime courant).
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn prx_11_upstream_failures_are_all_counted_but_logged_once_per_second() {
    let logs = Logs::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = fake_server().await;
    let addr = app.addr;
    drop(app);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let p = proxy(config(Some(addr), Mode::Record)).await;
    let started = Instant::now();
    for _ in 0..200 {
        let r = client().get(inbound(&p, "/users/1")).send().await.unwrap();
        assert_eq!(r.status(), 502);
    }
    let elapsed = started.elapsed();
    let errors: f64 = metrics(&p)
        .await
        .lines()
        .filter(|l| l.starts_with("mimic_upstream_errors_total{"))
        .filter_map(|l| l.rsplit_once(' ')?.1.parse::<f64>().ok())
        .sum();
    assert_eq!(errors, 200.0);
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("échec vers l'amont"))
        .collect();
    assert!(!lines.is_empty());
    assert!(
        lines.len() as u64 <= 1 + elapsed.as_secs(),
        "{} lignes en {elapsed:?}",
        lines.len()
    );
    if let Some(second) = lines.get(1) {
        assert!(!second.contains("omitted=0"), "{second}");
    }
}
