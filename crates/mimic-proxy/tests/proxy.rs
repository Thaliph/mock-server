//! Tests de bout en bout du proxy : PRX-*, MCK-* (bout en bout) et ADM-* (spec §11.2).

mod common;

use std::time::{Duration, Instant};

use common::*;
use mimic_core::model::{Body, Exchange};
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
    // Body binaire : gardé (tronqué) seulement par exception (spec §7).
    cfg.redact.raw_bodies = vec!["inbound".into()];
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
async fn cap_03_full_reservoir_bodies_are_neither_decoded_nor_redacted() {
    let app = fake_server().await;
    let mut cfg = config(Some(app.addr), Mode::Record);
    cfg.capture.samples_per_operation = 0;
    cfg.capture.max_body_bytes = 256;
    let p = proxy(cfg).await;
    let c = client();
    c.post(inbound(&p, "/echo"))
        .header("authorization", "Bearer top-secret-token")
        .json(&json!({"email": "jane@example.com"}))
        .send()
        .await
        .unwrap();
    c.post(inbound(&p, "/echo"))
        .json(&json!({"pad": "x".repeat(1000)}))
        .send()
        .await
        .unwrap();
    let list: Vec<Exchange> = wait_exchanges(&p, "direction=inbound", 2)
        .await
        .iter()
        .map(decode)
        .collect();
    for e in &list {
        assert!(
            matches!(e.req_body, Body::Skipped { .. }),
            "{:?}",
            e.req_body
        );
        assert!(
            matches!(e.resp_body, Body::Skipped { .. }),
            "{:?}",
            e.resp_body
        );
        assert!(
            e.redactions.iter().all(|m| !m.location.contains("body")),
            "body redacté puis jeté : {:?}",
            e.redactions
        );
    }
    // Les headers sont redactés dans tous les cas.
    assert!(list.iter().any(|e| {
        e.redactions
            .iter()
            .any(|m| m.location.contains("authorization"))
    }));
    let m = metrics(&p).await;
    let reason = |r: &str| {
        metric_value(
            &m,
            &format!("mimic_capture_dropped_total{{reason=\"{r}\"}}"),
        )
    };
    assert_eq!(reason("sample_limit"), Some(2.0));
    assert_eq!(
        reason("truncated"),
        None,
        "body tronqué alors qu'il est jeté"
    );
}

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

/// Les jauges du store ne sont pas tenues à chaque échange : calculées à la lecture de
/// `/metrics`, elles y sont exactes (ADM-05).
#[tokio::test]
async fn adm_05_store_gauges_are_exact_at_scrape() {
    let app = fake_server().await;
    let p = proxy(config(Some(app.addr), Mode::Record)).await;
    for i in 1..=5 {
        app_client(&p)
            .get(app.url(&format!("/users/{i}")))
            .send()
            .await
            .unwrap();
    }
    wait_exchanges(&p, "", 5).await;
    let m = metrics(&p).await;
    assert_eq!(
        metric_value(&m, r#"mimic_store_exchanges{origin="local"}"#),
        Some(5.0),
        "{m}"
    );
    let bytes = metric_value(&m, "mimic_store_bytes").unwrap();
    assert_eq!(bytes, p.state().store.bytes() as f64, "{m}");
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

/// Amont gRPC factice : HTTP/2 seulement (h2c), réponse suivie de trailers.
async fn fake_h2c_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use http_body_util::{BodyExt, StreamBody};
    use hyper::body::Frame;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |req: hyper::Request<hyper::body::Incoming>| async move {
                        let version = format!("{:?}", req.version());
                        let te = req
                            .headers()
                            .get("te")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let mut trailers = hyper::HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        let frames = vec![
                            Ok::<_, std::convert::Infallible>(Frame::data(body)),
                            Ok(Frame::trailers(trailers)),
                        ];
                        let resp = hyper::Response::builder()
                            .header("content-type", "application/grpc")
                            .header("x-upstream-version", version)
                            .header("x-upstream-te", te)
                            .body(StreamBody::new(futures_util::stream::iter(frames)))
                            .unwrap();
                        Ok::<_, std::convert::Infallible>(resp)
                    },
                );
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, handle)
}

#[tokio::test]
async fn prx_03_grpc_h2c_relayed_end_to_end_with_trailers() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let (upstream, _server) = fake_h2c_server().await;
    let p = proxy(config(Some(upstream), Mode::Record)).await;
    // Client gRPC : HTTP/2 en connaissance préalable, comme un client h2c.
    let stream = tokio::net::TcpStream::connect(p.inbound_addr())
        .await
        .unwrap();
    let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Full<Bytes>>(
        TokioExecutor::new(),
        TokioIo::new(stream),
    )
    .await
    .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .method("POST")
        .uri(format!(
            "http://{}/oteldemo.ShippingService/GetQuote",
            p.inbound_addr()
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(Bytes::from_static(b"\0\0\0\0\x02ok")))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.version(), http::Version::HTTP_2);
    assert_eq!(resp.headers()["x-upstream-version"], "HTTP/2.0");
    assert_eq!(resp.headers()["x-upstream-te"], "trailers");
    let collected = resp.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().expect("trailers relayés");
    assert_eq!(trailers["grpc-status"], "0");
    assert_eq!(&collected.to_bytes()[..], b"\0\0\0\0\x02ok");

    let list = wait_exchanges(&p, "direction=inbound", 1).await;
    let e = decode(&list[0]);
    assert_eq!(e.method, "POST");
    assert_eq!(e.status, 200);
    assert_eq!(e.path, "/oteldemo.ShippingService/GetQuote");
}

/// MCK-08 pour `br` et `zstd` (spec §5.2) : body décodé à l'enregistrement, réencodé au rejeu.
#[tokio::test]
async fn mck_08_brotli_and_zstd_responses_are_replayed() {
    let saas = fake_server().await;
    let p = proxy(config(None, Mode::Record)).await;
    let raw = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{}", p.outbound_addr())).unwrap())
        .no_gzip()
        .build()
        .unwrap();
    for path in ["/br", "/zstd"] {
        assert_eq!(raw.get(saas.url(path)).send().await.unwrap().status(), 200);
    }
    let list = wait_exchanges(&p, "direction=outbound", 2).await;
    for v in &list {
        let e = decode(v);
        assert!(
            matches!(e.resp_body, mimic_core::model::Body::Captured { .. }),
            "{} : {:?}",
            e.path,
            e.resp_body
        );
    }
    let hits = saas.hits();
    set_mode(&p, "mock").await;

    let r = raw.get(saas.url("/br")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-encoding"], "br");
    let mut out = String::new();
    std::io::Read::read_to_string(
        &mut brotli::Decompressor::new(&r.bytes().await.unwrap()[..], 4096),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, r#"{"compressed":"br"}"#);

    let r = raw.get(saas.url("/zstd")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-encoding"], "zstd");
    let out = zstd::decode_all(&r.bytes().await.unwrap()[..]).unwrap();
    assert_eq!(out, br#"{"compressed":"zstd"}"#);
    assert_eq!(saas.hits(), hits, "rien n'est sorti");
}
