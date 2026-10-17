//! Tests de bout en bout du proxy : PRX-*, MCK-* (bout en bout) et ADM-* (spec §11.2).

mod common;

use std::time::{Duration, Instant};

use common::*;
use mimic_core::model::Exchange;
use mimic_proxy::config::{Mode, Unmatched};
use serde_json::{Value, json};

fn decode(v: &Value) -> Exchange {
    serde_json::from_value(v.clone()).unwrap()
}

fn resp_text(e: &Exchange) -> String {
    String::from_utf8(e.resp_body.bytes().unwrap().to_vec()).unwrap()
}

#[tokio::test]
async fn prx_01_inbound_record_relays_and_records_everything() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    let c = client();
    for i in 0..200u64 {
        let r = c
            .get(inbound(&p, &format!("/users/{i}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-request-id"], format!("req-{i}").as_str());
        let body: Value = r.json().await.unwrap();
        assert_eq!(body, json!({"id": i, "name": format!("user{i}")}));
    }
    let list = wait_exchanges(&p, "direction=inbound&limit=1000", 200).await;
    assert_eq!(list.len(), 200);
    let e = decode(&list[0]);
    assert_eq!(e.method, "GET");
    assert_eq!(e.status, 200);
    assert!(e.total_us > 0);
    assert!(e.id.starts_with("test-0-"), "{}", e.id);
    assert_eq!(app.hits(), 200);
}

#[tokio::test]
async fn prx_01b_request_bodies_and_query_are_relayed_untouched() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    let c = client();
    let body = json!({"a": [1, 2, 3], "b": "é"});
    let r = c
        .post(inbound(&p, "/echo"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), body);
    let r = c
        .get(inbound(&p, "/search?q=a%20b&page=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"query": "q=a%20b&page=2"})
    );
    let list = wait_exchanges(&p, "direction=inbound", 2).await;
    let search = list
        .iter()
        .map(decode)
        .find(|e| e.path == "/search")
        .unwrap();
    assert_eq!(
        search.query,
        vec![("q".into(), "a b".into()), ("page".into(), "2".into())]
    );
}

#[tokio::test]
async fn prx_02_large_streamed_body_is_relayed_and_truncated_in_store() {
    let app = fake_server().await;
    let mut cfg = config(Some(app.addr), Mode::Record);
    cfg.capture.max_body_bytes = 1024;
    let p = proxy(cfg).await;
    let bytes = client()
        .get(inbound(&p, "/big"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.len(), 320 * 64 * 1024);
    let e = decode(&wait_exchanges(&p, "direction=inbound", 1).await[0]);
    match e.resp_body {
        mimic_core::model::Body::Truncated { size, prefix } => {
            assert_eq!(size, 320 * 64 * 1024);
            assert_eq!(prefix.len(), 1024);
        }
        other => panic!("body attendu tronqué, obtenu {other:?}"),
    }
}

#[tokio::test]
async fn prx_upstream_down_gives_502_and_metric() {
    let app = fake_server().await;
    let addr = app.addr;
    drop(app);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let p = proxy(config(Some(addr), Mode::Record)).await;
    let r = client().get(inbound(&p, "/users/1")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let m = metrics(&p).await;
    assert!(m.contains("mimic_upstream_errors_total{"), "{m}");
}

#[tokio::test]
async fn redaction_happens_before_storage() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    client()
        .post(inbound(&p, "/echo"))
        .header("authorization", "Bearer top-secret-token")
        .json(&json!({"password": "hunter2-secret"}))
        .send()
        .await
        .unwrap();
    wait_exchanges(&p, "direction=inbound", 1).await;
    let raw = client()
        .get(admin(&p, "/exchanges"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!raw.contains("top-secret-token"));
    let e = decode(&exchanges(&p, "").await[0]);
    assert!(!resp_text(&e).contains("hunter2-secret"));
    assert!(!e.redactions.is_empty());
}

/// Enregistre des appels sortants vers la dépendance, puis la coupe et passe en mock.
#[tokio::test]
async fn mck_e2e_record_then_mock_without_dependency() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Record)).await;
    let app = app_client(&p);
    for i in 1..=3u64 {
        app.get(saas.url(&format!("/users/{i}")))
            .send()
            .await
            .unwrap();
    }
    app.get(saas.url("/slow")).send().await.unwrap();
    app.get(saas.url("/fail")).send().await.unwrap();
    app.get(saas.url("/gz")).send().await.unwrap();
    wait_exchanges(&p, "direction=outbound", 6).await;
    let hits = saas.hits();
    saas.stop();
    set_mode(&p, "mock").await;

    // MCK-01 : réponse identique.
    let r = app.get(saas.url("/users/2")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-request-id"], "req-2");
    assert_eq!(r.headers()["x-mimic-match"], "exact;score=1.00");
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"id": 2, "name": "user2"})
    );

    // MCK-02 : un autre identifiant prend la réponse la plus proche.
    let r = app.get(saas.url("/users/42")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["x-mimic-match"]
            .to_str()
            .unwrap()
            .starts_with("template")
    );

    // Code d'erreur rejoué.
    assert_eq!(
        app.get(saas.url("/fail")).send().await.unwrap().status(),
        429
    );

    // MCK-04 : latence rejouée.
    let t = Instant::now();
    app.get(saas.url("/slow")).send().await.unwrap();
    assert!(
        t.elapsed() >= Duration::from_millis(90),
        "{:?}",
        t.elapsed()
    );

    // MCK-08 : encodage conservé, contenu identique une fois décodé.
    let raw = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", p.outbound_addr())).unwrap())
        .no_gzip()
        .build()
        .unwrap()
        .get(saas.url("/gz"))
        .send()
        .await
        .unwrap();
    assert_eq!(raw.headers()["content-encoding"], "gzip");
    let mut out = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(&raw.bytes().await.unwrap()[..]),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, r#"{"compressed":true}"#);

    // MCK-03 : requête inconnue.
    let r = app.get(saas.url("/nothing")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 599);
    assert_eq!(r.headers()["x-mimic-unmatched"], "true");

    // O3 : rien n'est sorti.
    assert_eq!(saas.hits(), hits);
    let m = metrics(&p).await;
    assert!(
        m.contains(r#"mimic_mock_responses_total{authority=""#),
        "{m}"
    );
    assert!(m.contains(r#"match="unmatched""#), "{m}");
    assert!(m.contains("mimic_mock_injected_latency_seconds"), "{m}");
}

#[tokio::test]
async fn mock_partial_forwards_and_records_unknown_requests() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::MockPartial)).await;
    let app = app_client(&p);
    app.get(saas.url("/users/1")).send().await.unwrap();
    assert_eq!(saas.hits(), 1);
    wait_exchanges(&p, "direction=outbound", 1).await;
    let r = app.get(saas.url("/users/1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mimic-match"));
    assert_eq!(saas.hits(), 1);
}

#[tokio::test]
async fn unmatched_passthrough_option_forwards() {
    let saas = fake_server().await;
    let mut cfg = config(None, Mode::Mock);
    cfg.mock.unmatched = Unmatched::Passthrough;
    let p = proxy(cfg).await;
    let r = app_client(&p)
        .get(saas.url("/users/9"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(saas.hits(), 1);
}

#[tokio::test]
async fn mock_mode_refuses_connect_tunnels() {
    let p = proxy(config(None, Mode::Mock)).await;
    let c = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{}", p.outbound_addr())).unwrap())
        .build()
        .unwrap();
    assert!(c.get("https://example.invalid/").send().await.is_err());
}

#[tokio::test]
async fn inbound_traffic_is_still_recorded_in_mock_mode() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Mock)).await;
    let r = client().get(inbound(&p, "/users/5")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    wait_exchanges(&p, "direction=inbound", 1).await;
}

#[tokio::test]
async fn prx_05_mode_switch_under_load_has_no_errors() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Record)).await;
    let app = app_client(&p);
    app.get(saas.url("/users/1")).send().await.unwrap();
    wait_exchanges(&p, "direction=outbound", 1).await;

    let load = {
        let app = app.clone();
        let url = saas.url("/users/1");
        tokio::spawn(async move {
            let mut statuses = vec![];
            for _ in 0..200 {
                statuses.push(app.get(&url).send().await.unwrap().status().as_u16());
            }
            statuses
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    set_mode(&p, "mock").await;
    let statuses = load.await.unwrap();
    assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
    let before = saas.hits();
    for _ in 0..10 {
        app.get(saas.url("/users/1")).send().await.unwrap();
    }
    assert_eq!(
        saas.hits(),
        before,
        "en mock, aucune requête ne doit sortir"
    );
}

#[tokio::test]
async fn passthrough_mode_records_nothing() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Passthrough)).await;
    client().get(inbound(&p, "/users/1")).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exchanges(&p, "").await.is_empty());
    let m = metrics(&p).await;
    assert!(m.contains(r#"mode="passthrough""#), "{m}");
}

#[tokio::test]
async fn adm_01_admin_page_lists_operations_and_required_params() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    let c = client();
    for i in 0..6 {
        c.get(inbound(&p, &format!("/users/{i}?page=1")))
            .send()
            .await
            .unwrap();
    }
    wait_exchanges(&p, "direction=inbound", 6).await;
    let html = c
        .get(admin(&p, "/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("/users/{userId}"), "{html}");
    assert!(html.contains("page"));
    assert!(html.contains("requis"));
    assert!(html.contains("<html"));

    let ops: Value = c
        .get(admin(&p, "/api/operations"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let op = ops
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["path"] == "/users/{userId}")
        .unwrap();
    assert_eq!(op["direction"], "inbound");
    assert_eq!(op["method"], "GET");
    assert_eq!(op["count"], 6);
    let page = op["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "page")
        .unwrap();
    assert_eq!(page["required"], true);

    let doc: Value = c
        .get(admin(&p, "/openapi/inbound"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    assert!(doc["paths"]["/users/{userId}"]["get"].is_object());
}

#[tokio::test]
async fn adm_02_metrics_cover_record_and_mock() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    client().get(inbound(&p, "/users/1")).send().await.unwrap();
    app_client(&p)
        .get(app.url("/users/1"))
        .send()
        .await
        .unwrap();
    wait_exchanges(&p, "", 2).await;
    set_mode(&p, "mock").await;
    app_client(&p)
        .get(app.url("/users/1"))
        .send()
        .await
        .unwrap();
    // Le calcul des jauges de découverte se fait à la lecture.
    client()
        .get(admin(&p, "/api/operations"))
        .send()
        .await
        .unwrap();
    let m = metrics(&p).await;
    for needle in [
        r#"mimic_requests_total{direction="inbound",mode="record""#,
        r#"mimic_requests_total{direction="outbound",mode="record""#,
        r#"mimic_requests_total{direction="outbound",mode="mock""#,
        "mimic_request_duration_seconds_bucket{",
        r#"mimic_mock_responses_total{authority=""#,
        r#"mimic_discovered_operations{direction="inbound""#,
        r#"mimic_store_exchanges{origin="local"}"#,
        "mimic_mode{",
    ] {
        assert!(m.contains(needle), "{needle} absent de\n{m}");
    }
}

#[tokio::test]
async fn adm_03_mode_api_switches_and_reports() {
    let p = proxy(config(None, Mode::Record)).await;
    let c = client();
    assert_eq!(
        c.get(admin(&p, "/mode"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "record"
    );
    set_mode(&p, "mock").await;
    assert_eq!(
        c.get(admin(&p, "/mode"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "mock"
    );
    let m = metrics(&p).await;
    assert_eq!(
        metric_value(&m, r#"mimic_mode{mode="mock"}"#),
        Some(1.0),
        "{m}"
    );
    assert_eq!(
        metric_value(&m, r#"mimic_mode{mode="record"}"#),
        Some(0.0),
        "{m}"
    );
    let bad = c.put(admin(&p, "/mode")).body("nope").send().await.unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn mode_follows_pod_annotation_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("annotations");
    std::fs::write(
        &file,
        "kubernetes.io/config.seen=\"x\"\nmimic.platform.io/mode=\"record\"\n",
    )
    .unwrap();
    let mut cfg = config(None, Mode::Record);
    cfg.annotations_file = Some(file.clone());
    let p = proxy(cfg).await;
    std::fs::write(&file, "mimic.platform.io/mode=\"mock\"\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mode = client()
            .get(admin(&p, "/mode"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if mode == "mock" {
            break;
        }
        assert!(Instant::now() < deadline, "mode = {mode}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn store_is_persisted_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let saas = fake_server().await;
    let mut cfg = config(None, Mode::Record);
    cfg.store.dir = Some(dir.path().to_path_buf());
    let p = proxy(cfg.clone()).await;
    app_client(&p)
        .get(saas.url("/users/3"))
        .send()
        .await
        .unwrap();
    wait_exchanges(&p, "direction=outbound", 1).await;
    p.shutdown().await;

    saas.stop();
    cfg.mode = Mode::Mock;
    let p = proxy(cfg).await;
    let r = app_client(&p)
        .get(saas.url("/users/3"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], 3);
}

#[tokio::test]
async fn health_endpoints() {
    let p = proxy(config(None, Mode::Record)).await;
    for path in ["/healthz", "/readyz"] {
        assert_eq!(
            client().get(admin(&p, path)).send().await.unwrap().status(),
            200
        );
    }
}
