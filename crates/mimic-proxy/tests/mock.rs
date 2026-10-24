//! Critères MCK-16, MCK-17, MCK-22 et RED-05 (spec §5.3, §7 et §11.2) : moteur de mock
//! construit à la bascule, servi sans reconstruction, et bodies rejoués octet pour octet.

mod common;

use std::time::Duration;

use common::*;
use mimic_core::model::{Body, Direction, Exchange, Header};
use mimic_proxy::config::Mode;
use mimic_proxy::store::Origin;

/// Dépendance jamais jointe : le mock seul y répond.
const DEP: &str = "dep.test:80";

fn recorded(path: &str, id: &str, body: &str) -> Exchange {
    Exchange {
        id: id.into(),
        direction: Direction::Outbound,
        started_at_ms: 1_700_000_000_000,
        scheme: "http".into(),
        authority: DEP.into(),
        method: "GET".into(),
        path: path.into(),
        query: vec![],
        req_headers: vec![],
        req_body: Body::Empty,
        status: 200,
        resp_headers: vec![Header::new("content-type", "application/json")],
        resp_body: Body::captured(body.as_bytes().to_vec()),
        ttfb_us: 100,
        total_us: 100,
        trace_id: None,
        resp_trailers: vec![],
        redactions: vec![],
    }
}

fn builds(m: &str, trigger: &str) -> f64 {
    metric_value(
        m,
        &format!("mimic_mock_engine_builds_total{{trigger=\"{trigger}\"}}"),
    )
    .unwrap_or(0.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_16_engine_is_built_once_at_switch() {
    let p = proxy(config(None, Mode::Record)).await;
    for i in 0..2_000 {
        let path = format!("/users/{}", i % 50 + 1);
        p.state().store.add(
            recorded(&path, &format!("peer-{i:05}"), &format!(r#"{{"n":{i}}}"#)),
            Origin::Peer,
        );
    }
    set_mode(&p, "mock").await;
    let m = metrics(&p).await;
    assert_eq!(builds(&m, "switch"), 1.0, "{m}");

    let app = app_client(&p);
    let mut tasks = vec![];
    for i in 0..200 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            app.get(format!("http://dep.test/users/{}", i % 50 + 1))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let m = metrics(&p).await;
    assert_eq!(builds(&m, "switch"), 1.0, "{m}");
    assert_eq!(builds(&m, "refresh"), 0.0, "{m}");
    assert_eq!(builds(&m, "unmatched"), 0.0, "{m}");

    // Retour en record : moteur libéré ; nouvelle bascule : nouvelle construction.
    set_mode(&p, "record").await;
    set_mode(&p, "mock").await;
    assert_eq!(builds(&metrics(&p).await, "switch"), 2.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_17_store_changes_do_not_block_matched_requests() {
    let p = proxy(config(None, Mode::Record)).await;
    for i in 1..=50 {
        p.state().store.add(
            recorded(&format!("/users/{i}"), &format!("a-{i:03}"), r#"{"a":1}"#),
            Origin::Peer,
        );
    }
    set_mode(&p, "mock").await;
    let app = app_client(&p);
    let get = |path: &str| {
        let app = app.clone();
        let url = format!("http://dep.test{path}");
        async move { app.get(url).send().await.unwrap() }
    };
    for _ in 0..20 {
        assert_eq!(get("/users/1").await.status(), 200);
    }
    // Import d'un pair : le moteur courant est périmé.
    p.state().store.add(
        recorded("/orders/7", "b-001", r#"{"order":7}"#),
        Origin::Peer,
    );
    for _ in 0..20 {
        assert_eq!(get("/users/2").await.status(), 200);
    }
    let m = metrics(&p).await;
    assert_eq!(builds(&m, "unmatched"), 0.0, "{m}");
    // Une requête sans correspondance dans le moteur périmé attend un moteur à jour.
    let r = get("/orders/7").await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), r#"{"order":7}"#);
    // Servie par l'index des ajouts (MCK-22) : la reconstruction se fait en tâche de fond.
    let m = metrics(&p).await;
    assert_eq!(builds(&m, "unmatched"), 0.0, "{m}");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while builds(&metrics(&p).await, "refresh") < 1.0 {
        assert!(
            std::time::Instant::now() < deadline,
            "reconstruction en tâche de fond"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Toujours rien pour une requête inconnue.
    assert_eq!(get("/nothing").await.status().as_u16(), 599);

    // mock-partial : relayée et enregistrée, puis mockée dès la seconde fois.
    let saas = fake_server().await;
    set_mode(&p, "mock-partial").await;
    let r = app.get(saas.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(saas.hits(), 1);
    wait_exchanges(&p, "direction=outbound&limit=1000", 52).await;
    let r = app.get(saas.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mimic-match"));
    assert_eq!(saas.hits(), 1);
}

#[tokio::test]
async fn red_05_untouched_json_is_replayed_byte_for_byte() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Record)).await;
    let app = app_client(&p);
    let raw = r#"{ "zeta": 1.0, "alpha": [2, 3],  "id": 12 }"#;
    let r = app
        .post(saas.url("/echo"))
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), raw);
    wait_exchanges(&p, "direction=outbound", 1).await;
    saas.stop();
    set_mode(&p, "mock").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let r = app
        .post(saas.url("/echo"))
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-length"],
        raw.len().to_string().as_str()
    );
    assert_eq!(r.text().await.unwrap(), raw);
}

/// mock-partial : un échange relayé puis enregistré ne reconstruit pas le moteur ; la même
/// requête est mockée dès la suivante (spec §5.3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mck_22_relayed_exchanges_do_not_rebuild_the_engine() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Record)).await;
    for i in 0..2_000 {
        let path = format!("/users/{}", i % 50 + 1);
        p.state().store.add(
            recorded(&path, &format!("peer-{i:05}"), &format!(r#"{{"n":{i}}}"#)),
            Origin::Peer,
        );
    }
    set_mode(&p, "mock-partial").await;
    let started = std::time::Instant::now();
    let app = app_client(&p);
    for i in 0..200 {
        // Opération inconnue : relayée, puis enregistrée.
        let url = saas.url(&format!("/items{i}"));
        let r = app.get(&url).send().await.unwrap();
        assert!(!r.headers().contains_key("x-mimic-match"));
        let stored = 2_001 + i;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while p.state().store.len() < stored {
            assert!(
                std::time::Instant::now() < deadline,
                "échange {i} enregistré"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // La même : mockée, sans nouvel appel à l'amont.
        let r = app.get(&url).send().await.unwrap();
        assert!(r.headers().contains_key("x-mimic-match"), "requête {i}");
        assert_eq!(saas.hits(), i + 1);
        // Avec correspondance : servie par le moteur en service.
        let r = app
            .get(format!("http://dep.test/users/{}", i % 50 + 1))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let m = metrics(&p).await;
    assert_eq!(builds(&m, "switch"), 1.0, "{m}");
    assert_eq!(builds(&m, "unmatched"), 0.0, "{m}");
    assert!(
        builds(&m, "refresh") <= elapsed.ceil() + 1.0,
        "{elapsed} s : {m}"
    );
    assert_eq!(saas.hits(), 200);
}
