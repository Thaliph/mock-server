//! Requêtes sans réponse du mock visibles (spec §5.3, §9.2, §9.3) : `599` et `413` comptés,
//! journalisés en débit limité et relevés par `GET /api/unmatched` (MCK-26, MCK-27, ADM-07).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use mimic_core::snapshot::{MockSnapshot, Snapshot};
use mimic_proxy::Running;
use mimic_proxy::config::Mode;
use serde_json::Value;

/// Journal de test : les lignes de log émises sur ce fil (runtime courant).
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
        let logs = Logs::default();
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn lines(&self, needle: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(needle))
            .map(String::from)
            .collect()
    }
}

async fn unmatched(p: &Running, query: &str) -> Value {
    let r = client()
        .get(admin(p, &format!("/api/unmatched{query}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

fn paths(report: &Value) -> Vec<String> {
    report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {}",
                e["method"].as_str().unwrap(),
                e["path"].as_str().unwrap()
            )
        })
        .collect()
}

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

#[tokio::test]
async fn mck_26_refused_requests_are_counted_logged_and_listed() {
    let (logs, _guard) = Logs::capture();
    let saas = fake_server().await;
    let mut c = config(None, Mode::Record);
    c.capture.max_body_bytes = 1024;
    let p = proxy(c).await;
    let app = app_client(&p);
    assert_eq!(
        app.get(saas.url("/users/1")).send().await.unwrap().status(),
        200
    );
    wait_exchanges(&p, "direction=outbound", 1).await;
    set_mode(&p, "mock").await;

    // Sans correspondance : 599, relevé avec la forme du chemin.
    let r = app
        .get(saas.url("/orders/42?email=a@b.io"))
        .header("traceparent", TRACEPARENT)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 599);
    assert_eq!(r.headers()["x-mimic-unmatched"], "true");
    // Body trop gros pour le mock : 413.
    let r = app
        .post(saas.url("/echo"))
        .body(vec![b'x'; 2048])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
    for n in 0..50 {
        let r = app
            .get(saas.url(&format!("/orders/{n}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 599);
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = app.get(saas.url("/orders/77")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 599);
    // Avec correspondance (même forme que l'enregistrement) : ni relevée, ni journalisée.
    let r = app.get(saas.url("/users/7")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let authority = saas.addr.to_string();
    let m = metrics(&p).await;
    let mock = |kind: &str| {
        metric_value(
            &m,
            &format!("mimic_mock_responses_total{{authority=\"{authority}\",match=\"{kind}\"}}"),
        )
    };
    assert_eq!(mock("unmatched"), Some(52.0), "{m}");
    assert_eq!(mock("too_large"), Some(1.0), "{m}");
    let requests = |method: &str, status: u16| {
        metric_value(
            &m,
            &format!(
                "mimic_requests_total{{direction=\"outbound\",mode=\"mock\",authority=\"{authority}\",method=\"{method}\",status=\"{status}\"}}"
            ),
        )
    };
    assert_eq!(requests("GET", 599), Some(52.0), "{m}");
    assert_eq!(requests("POST", 413), Some(1.0), "{m}");

    let report = unmatched(&p, "").await;
    assert_eq!(report["total"], 53, "{report}");
    assert_eq!(
        paths(&report),
        ["GET /orders/{int}", "POST /echo"],
        "{report}"
    );
    let orders = &report["entries"][0];
    assert_eq!(orders["authority"], authority.as_str());
    assert_eq!(orders["reason"], "unmatched");
    assert_eq!(orders["count"], 52);
    assert!(orders["first_ms"].as_u64().unwrap() <= orders["last_ms"].as_u64().unwrap());
    assert_eq!(report["entries"][1]["reason"], "too_large");
    let listed = serde_json::to_string(&report["entries"]).unwrap();
    for value in ["/orders/42", "a@b.io", "email", "?"] {
        assert!(!listed.contains(value), "{value} dans {listed}");
    }

    // Journal : une ligne par raison, puis une après la seconde, avec les lignes omises.
    let lines = logs.lines("requête sans réponse enregistrée");
    assert_eq!(lines.len(), 2, "{}", logs.text());
    assert!(lines.iter().all(|l| l.contains("WARN")), "{lines:?}");
    assert!(lines[0].contains("/orders/{int}"), "{}", lines[0]);
    assert!(lines[0].contains("GET"), "{}", lines[0]);
    assert!(lines[0].contains(&authority), "{}", lines[0]);
    assert!(
        lines[0].contains("4bf92f3577b34da6a3ce929d0e0e4736"),
        "{}",
        lines[0]
    );
    assert!(lines[1].contains("omitted=50"), "{}", lines[1]);
    let too_large = logs.lines("body de requête trop gros");
    assert_eq!(too_large.len(), 1, "{}", logs.text());
    assert!(too_large[0].contains("/echo") && too_large[0].contains("POST"));
    let text = logs.text();
    for value in ["/orders/42", "a@b.io", "/users/7"] {
        assert!(!text.contains(value), "{value} dans le journal :\n{text}");
    }
}

#[tokio::test]
async fn adm_07_listing_is_bounded_ordered_and_local_only() {
    let mut c = config(None, Mode::Mock);
    c.admin.listen = "0.0.0.0:0".parse().unwrap();
    let p = proxy(c).await;
    let app = app_client(&p);
    // Par lots de 10, séparés de quelques ms : les lots ont des horodatages distincts.
    for n in 0..150 {
        let r = app
            .get(format!("http://api.saas.io/p{n}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 599);
        if n % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    }
    let port = p.admin_addr().port();
    let url = format!("http://127.0.0.1:{port}/api/unmatched");
    let report: Value = client()
        .get(&url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["total"], 150);
    let entries = report["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 100);
    let last: Vec<u64> = entries
        .iter()
        .map(|e| e["last_ms"].as_u64().unwrap())
        .collect();
    assert!(last.windows(2).all(|w| w[0] >= w[1]), "{last:?}");
    let mut kept: Vec<String> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    kept.sort();
    let mut want: Vec<String> = (50..150).map(|n| format!("/p{n}")).collect();
    want.sort();
    assert_eq!(kept, want, "les 100 clés les plus récentes");

    let five: Value = client()
        .get(format!("{url}?limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(five["total"], 150);
    assert_eq!(five["entries"].as_array().unwrap()[..], entries[..5]);

    let remote = reqwest::Client::builder()
        .no_proxy()
        .local_address(std::net::IpAddr::from([127, 0, 0, 6]))
        .build()
        .unwrap();
    assert_eq!(remote.get(&url).send().await.unwrap().status(), 403);
}

#[tokio::test]
async fn mck_27_listing_survives_record_and_is_reset_by_the_next_mock() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Mock)).await;
    let app = app_client(&p);
    let refused = |path: &str| {
        let (app, url) = (app.clone(), saas.url(path));
        async move { app.get(url).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(refused("/a/1").await, 599);
    assert_eq!(unmatched(&p, "").await["total"], 1);

    // En record, le relevé du tir reste lisible.
    p.state().set_mode(Mode::Record);
    assert_eq!(unmatched(&p, "").await["total"], 1);
    // Retour en mock : nouveau tir, relevé vide.
    p.state().set_mode(Mode::Mock);
    assert_eq!(unmatched(&p, "").await["total"], 0);
    assert_eq!(refused("/b/2").await, 599);
    // Même mode : rien n'est remis à zéro.
    p.state().set_mode(Mode::Mock);
    assert_eq!(unmatched(&p, "").await["total"], 1);

    // Un autre snapshot : relevé vide.
    let snap = |id: &str| {
        Some(Arc::new(MockSnapshot::from_snapshot(&Snapshot {
            id: id.into(),
            exchanges: vec![],
        })))
    };
    p.state().set_snapshot(snap("s1"));
    assert_eq!(unmatched(&p, "").await["total"], 0);
    assert_eq!(refused("/c/3").await, 599);
    p.state().set_snapshot(snap("s1"));
    assert_eq!(unmatched(&p, "").await["total"], 1, "même snapshot");
    p.state().set_snapshot(snap("s2"));
    assert_eq!(unmatched(&p, "").await["total"], 0);

    // mock-partial : la requête relayée a reçu la réponse de la dépendance.
    p.state().set_mode(Mode::MockPartial);
    p.state().set_snapshot(None);
    assert_eq!(refused("/users/3").await, 200);
    let report = unmatched(&p, "").await;
    assert_eq!(report["total"], 0, "{report}");
}
