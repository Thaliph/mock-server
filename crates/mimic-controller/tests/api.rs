//! Critères CTL-*, SEC-02, SEC-03, SEC-06 et RPL-* de la spec (§5.5 à §5.7, §7.1),
//! au niveau de l'API du contrôleur.

use std::collections::BTreeMap;

use mimic_controller::config::{AuthConfig, ControllerConfig, Identity};
use mimic_controller::reconcile::MimicWorkloadSpec;
use mimic_controller::{Running, start};
use serde_json::{Value, json};

const WL: &str = "/v1/workloads/shop/checkout";

fn identity(ns: &str, pod: &str, workload: &str) -> Identity {
    Identity {
        namespace: ns.into(),
        pod: pod.into(),
        workload: workload.into(),
        ..Identity::default()
    }
}

async fn controller(dir: &std::path::Path) -> Running {
    let tokens = BTreeMap::from([
        (
            "tok-a".to_string(),
            identity("shop", "checkout-a", "checkout"),
        ),
        (
            "tok-b".to_string(),
            identity("shop", "checkout-b", "checkout"),
        ),
        ("tok-web".to_string(), identity("shop", "web-a", "web")),
        (
            "tok-other-ns".to_string(),
            identity("bank", "checkout-a", "checkout"),
        ),
    ]);
    start(ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ui_listen: "127.0.0.1:0".parse().unwrap(),
        store: format!("file://{}", dir.display()),
        auth: AuthConfig::Static { tokens },
        ..ControllerConfig::default()
    })
    .await
    .unwrap()
}

/// Route de l'API des sidecars.
fn url(c: &Running, path: &str) -> String {
    format!("http://{}{path}", c.api_addr())
}

/// Route de l'interface en lecture seule.
fn ui(c: &Running, path: &str) -> String {
    format!("http://{}{path}", c.ui_addr())
}

fn token_of(instance: &str) -> &'static str {
    match instance {
        "checkout-a" => "tok-a",
        "checkout-b" => "tok-b",
        _ => "inconnu",
    }
}

async fn set_mode(c: &Running, mode: &str, snapshot: Option<&str>) -> Value {
    c.apply_spec(
        "shop",
        "checkout",
        Some(MimicWorkloadSpec {
            mode: mode.into(),
            snapshot: snapshot.map(Into::into),
            ..MimicWorkloadSpec::default()
        }),
    )
    .await;
    get_json(c, &format!("{WL}/mode")).await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// Échange minimal au format du store, tel qu'un sidecar l'envoie.
fn exchange(id: &str, direction: &str, method: &str, path: &str, query: Value, at: u64) -> Value {
    json!({
        "id": id, "direction": direction, "started_at_ms": at, "scheme": "http",
        "authority": if direction == "outbound" { "api.saas.io:443" } else { "checkout:8080" },
        "method": method, "path": path, "query": query,
        "req_headers": [{"name": "authorization", "value": "<redacted:sha256:0123abcd>"}],
        "req_body": {"kind": "empty"},
        "status": 200,
        "resp_headers": [{"name": "content-type", "value": "application/json"}],
        "resp_body": {"kind": "captured", "data": "eyJpZCI6N30="},
        "ttfb_us": 1000, "total_us": 2000, "trace_id": null
    })
}

async fn push(c: &Running, instance: &str, exchanges: &[Value]) {
    let body: String = exchanges.iter().map(|e| format!("{e}\n")).collect();
    let r = client()
        .post(url(c, &format!("{WL}/segments?instance={instance}")))
        .bearer_auth(token_of(instance))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
}

/// Lecture sur l'interface.
async fn get_json(c: &Running, path: &str) -> Value {
    let r = client().get(ui(c, path)).send().await.unwrap();
    assert!(r.status().is_success(), "{path}: {}", r.status());
    r.json().await.unwrap()
}

async fn heartbeat(c: &Running, instance: &str, mode: &str, snapshot: Option<&str>) -> Value {
    client()
        .post(url(c, &format!("{WL}/heartbeat")))
        .bearer_auth(token_of(instance))
        .json(&json!({"instance": instance, "mode": mode, "snapshot": snapshot}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn segments_are_listed_and_served() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    push(
        &c,
        "checkout-a",
        &[exchange(
            "checkout-a-1",
            "inbound",
            "GET",
            "/a",
            json!([]),
            1,
        )],
    )
    .await;
    let list: Value = client()
        .get(url(&c, &format!("{WL}/segments")))
        .bearer_auth("tok-b")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let seg = &list.as_array().unwrap()[0];
    assert_eq!(seg["instance"], "checkout-a");
    let key = seg["key"].as_str().unwrap();
    let body = client()
        .get(url(&c, &format!("{WL}/segments/{key}")))
        .bearer_auth("tok-b")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("checkout-a-1"));
}

#[tokio::test]
async fn invalid_segments_and_instances_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    let r = client()
        .post(url(&c, &format!("{WL}/segments?instance=checkout-a")))
        .bearer_auth("tok-a")
        .body("pas du json\n")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = client()
        .post(url(&c, &format!("{WL}/segments?instance=../x")))
        .bearer_auth("tok-a")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn without_resource_sidecars_are_free() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    assert_eq!(
        heartbeat(&c, "checkout-a", "record", None).await,
        json!({"mode": null, "snapshot": null})
    );
    // L'interface ne permet aucune écriture.
    let r = client()
        .put(ui(&c, &format!("{WL}/mode")))
        .json(&json!({"mode": "mock"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error());
    let r = client()
        .put(url(&c, &format!("{WL}/mode")))
        .bearer_auth("tok-a")
        .json(&json!({"mode": "mock"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error());
}

#[tokio::test]
async fn sec_02_missing_or_invalid_token_is_401() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    let r = client()
        .get(url(&c, &format!("{WL}/segments")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = client()
        .post(url(&c, &format!("{WL}/heartbeat")))
        .bearer_auth("faux")
        .json(&json!({"instance": "checkout-a", "mode": "record", "snapshot": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = client().get(url(&c, "/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let m = client()
        .get(ui(&c, "/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        m.contains(r#"mimic_controller_auth_failures_total{reason="missing"} 1"#),
        "{m}"
    );
    assert!(
        m.contains(r#"mimic_controller_auth_failures_total{reason="invalid"} 1"#),
        "{m}"
    );
}

#[tokio::test]
async fn sec_03_token_is_bound_to_its_pod_and_workload() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    let body = format!(
        "{}\n",
        exchange("checkout-a-1", "inbound", "GET", "/a", json!([]), 1)
    );
    // Autre workload du même namespace.
    let r = client()
        .post(url(&c, &format!("{WL}/segments?instance=web-a")))
        .bearer_auth("tok-web")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // Même nom de workload, autre namespace.
    let r = client()
        .get(url(&c, &format!("{WL}/segments")))
        .bearer_auth("tok-other-ns")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // Bon workload, mais au nom d'une autre instance.
    let r = client()
        .post(url(&c, &format!("{WL}/segments?instance=checkout-b")))
        .bearer_auth("tok-a")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = client()
        .post(url(&c, &format!("{WL}/heartbeat")))
        .bearer_auth("tok-a")
        .json(&json!({"instance": "checkout-b", "mode": "record", "snapshot": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn sec_06_ui_is_bound_to_loopback_by_default() {
    let config = ControllerConfig::default();
    assert!(config.ui_listen.ip().is_loopback());
}

#[tokio::test]
async fn ctl_02_mock_builds_a_snapshot_and_heartbeats_report_alignment() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    push(
        &c,
        "checkout-a",
        &[exchange(
            "checkout-a-1",
            "outbound",
            "GET",
            "/users/7",
            json!([]),
            1,
        )],
    )
    .await;
    let desired = set_mode(&c, "mock", None).await;
    assert_eq!(desired["mode"], "mock");
    let id = desired["snapshot"].as_str().unwrap().to_string();
    assert_eq!(heartbeat(&c, "checkout-a", "record", None).await, desired);
    heartbeat(&c, "checkout-b", "mock", Some(&id)).await;
    let wl = get_json(&c, "/v1/workloads").await;
    let pods = &wl[0]["pods"];
    let state = |i: &str| {
        pods.as_array()
            .unwrap()
            .iter()
            .find(|p| p["instance"] == i)
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(state("checkout-a"), "diverging");
    assert_eq!(state("checkout-b"), "aligned");
    let snap = client()
        .get(url(&c, &format!("{WL}/snapshots/{id}")))
        .bearer_auth("tok-b")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    mimic_core::snapshot::Snapshot::from_jsonl(&snap, &id).unwrap();

    // CTL-03 : retour en record, sans snapshot.
    let desired = set_mode(&c, "record", None).await;
    assert_eq!(desired, json!({"mode": "record", "snapshot": null}));
    // SNP-04 : un snapshot épinglé est rejoué tel quel.
    push(
        &c,
        "checkout-a",
        &[exchange(
            "checkout-a-2",
            "outbound",
            "GET",
            "/users/8",
            json!([]),
            2,
        )],
    )
    .await;
    let desired = set_mode(&c, "mock", Some(&id)).await;
    assert_eq!(desired["snapshot"], id.as_str());
}

#[tokio::test]
async fn ctl_04_restart_keeps_segments_snapshots_and_desired_state() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    push(
        &c,
        "checkout-a",
        &[
            exchange("checkout-a-1", "outbound", "GET", "/users/7", json!([]), 1),
            exchange(
                "checkout-a-2",
                "inbound",
                "GET",
                "/orders",
                json!([["page", "1"]]),
                2,
            ),
        ],
    )
    .await;
    let desired = set_mode(&c, "mock", None).await;
    c.shutdown().await;

    let c = controller(dir.path()).await;
    assert_eq!(get_json(&c, &format!("{WL}/mode")).await, desired);
    let id = desired["snapshot"].as_str().unwrap();
    let r = client()
        .get(ui(&c, &format!("{WL}/snapshots/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let snaps = get_json(&c, &format!("{WL}/snapshots")).await;
    assert_eq!(snaps[0]["id"], id);
    let ops = get_json(&c, &format!("{WL}/operations")).await;
    assert!(
        ops.as_array()
            .unwrap()
            .iter()
            .any(|o| o["path"] == "/orders")
    );
    let wl = get_json(&c, "/v1/workloads").await;
    assert_eq!(wl[0]["namespace"], "shop");
    assert_eq!(wl[0]["name"], "checkout");
}

#[tokio::test]
async fn ctl_05_workload_page_shows_operations_pods_and_switch() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    let reqs: Vec<Value> = (0..5)
        .map(|i| {
            exchange(
                &format!("checkout-a-{i}"),
                "inbound",
                "GET",
                &format!("/users/{}", 10 + i),
                json!([["page", i.to_string()]]),
                i,
            )
        })
        .collect();
    push(&c, "checkout-a", &reqs).await;
    heartbeat(&c, "checkout-a", "record", None).await;
    let html = client()
        .get(ui(&c, "/ui/shop/checkout"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("/users/{userId}"), "{html}");
    assert!(html.contains("requis"));
    assert!(html.contains("checkout-a"));
    assert!(html.contains("kubectl patch mimicworkload checkout -n shop"));
    let home = client()
        .get(ui(&c, "/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(home.contains("/ui/shop/checkout"));
}

#[tokio::test]
async fn rpl_01_to_03_recorded_requests_in_every_format() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    push(
        &c,
        "checkout-a",
        &[exchange(
            "checkout-a-1",
            "inbound",
            "GET",
            "/a",
            json!([]),
            1,
        )],
    )
    .await;
    push(
        &c,
        "checkout-b",
        &[
            exchange(
                "checkout-b-1",
                "inbound",
                "POST",
                "/b",
                json!([["q", "x"]]),
                2,
            ),
            exchange("checkout-b-2", "outbound", "GET", "/users/7", json!([]), 3),
        ],
    )
    .await;

    let list = get_json(&c, &format!("{WL}/requests?direction=inbound")).await;
    let summary: Vec<(String, String)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["instance"].as_str().unwrap().into(),
                r["path"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("checkout-b".into(), "/b".into()),
            ("checkout-a".into(), "/a".into())
        ]
    );

    let jsonl = client()
        .get(ui(
            &c,
            &format!("{WL}/requests?direction=inbound&format=jsonl"),
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let lines: Vec<mimic_core::model::Exchange> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);

    let r = client()
        .get(ui(
            &c,
            &format!("{WL}/requests?direction=inbound&format=k6"),
        ))
        .send()
        .await
        .unwrap();
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    let script = r.text().await.unwrap();
    assert!(script.contains("k6/http"));
    assert!(script.contains("\"/b\""));
    assert!(!script.contains("<redacted:"));
}

#[tokio::test]
async fn metrics_and_health() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    push(
        &c,
        "checkout-a",
        &[exchange(
            "checkout-a-1",
            "inbound",
            "GET",
            "/a",
            json!([]),
            1,
        )],
    )
    .await;
    heartbeat(&c, "checkout-a", "record", None).await;
    let r = client().get(url(&c, "/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let m = client()
        .get(ui(&c, "/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        m.contains(r#"mimic_controller_segments_total{result="ok"} 1"#),
        "{m}"
    );
    assert!(m.contains("mimic_controller_workloads 1"), "{m}");
    assert!(
        m.contains(r#"mimic_controller_pods{state="aligned"} 1"#),
        "{m}"
    );
}

/// Forme de mock telle qu'un pod la télécharge : code, headers et body (non décodé).
async fn mock_form(
    c: &Running,
    id: &str,
    token: &str,
    accept: Option<&str>,
    if_none_match: Option<&str>,
) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let mut req = client()
        .get(url(c, &format!("{WL}/snapshots/{id}/mock")))
        .bearer_auth(token);
    if let Some(a) = accept {
        req = req.header("accept-encoding", a);
    }
    if let Some(e) = if_none_match {
        req = req.header("if-none-match", e);
    }
    let r = req.send().await.unwrap();
    let (status, headers) = (r.status().as_u16(), r.headers().clone());
    (status, headers, r.bytes().await.unwrap().to_vec())
}

fn header<'a>(h: &'a reqwest::header::HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).map(|v| v.to_str().unwrap())
}

#[tokio::test]
async fn ctl_23_mock_form_is_light_compressed_and_identical_for_every_pod() {
    use mimic_core::snapshot::{MockSnapshot, Snapshot};
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    // 2 000 appels d'un même endpoint, sur des chemins distincts de même forme : seuls les
    // premiers gardent leur body en mémoire (samples_per_operation), tous entrent dans le
    // snapshot (un par chemin).
    let calls: Vec<Value> = (0..2_000u64)
        .map(|i| {
            let mut e = exchange(
                &format!("checkout-a-{i:05}"),
                "outbound",
                "GET",
                &format!("/users/{i}"),
                json!([["page", "1"]]),
                1_000 + i,
            );
            e["total_us"] = json!(2_000 + i % 700);
            e
        })
        .collect();
    push(&c, "checkout-a", &calls).await;
    let id = set_mode(&c, "mock", None).await["snapshot"]
        .as_str()
        .unwrap()
        .to_string();

    // Chemin d'avant : snapshot complet, octet pour octet celui du support.
    let full = client()
        .get(url(&c, &format!("{WL}/snapshots/{id}")))
        .bearer_auth("tok-a")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    let stored = std::fs::read(
        dir.path()
            .join(format!("shop/checkout/snapshots/{id}.jsonl")),
    )
    .unwrap();
    assert_eq!(full, stored);
    let snapshot = Snapshot::from_jsonl(&full, &id).unwrap();
    assert_eq!(snapshot.exchanges.len(), 2_000);
    let expected = MockSnapshot::from_snapshot(&snapshot).to_json();

    // Deux pods, trois encodages : mêmes octets, même ETag, même forme une fois décodée.
    let (status, h, plain) = mock_form(&c, &id, "tok-a", None, None).await;
    assert_eq!(status, 200);
    assert_eq!(header(&h, "content-encoding"), None);
    assert_eq!(header(&h, "content-type"), Some("application/json"));
    assert_eq!(plain, expected);
    let etag = header(&h, "etag").unwrap().to_string();
    assert_eq!(
        etag,
        format!("\"{}\"", mimic_core::snapshot::digest(&expected))
    );
    assert!(
        header(&h, "cache-control").is_some_and(|v| v.contains("immutable")),
        "{h:?}"
    );

    let (_, hz, zst) = mock_form(&c, &id, "tok-a", Some("gzip, zstd"), None).await;
    assert_eq!(header(&hz, "content-encoding"), Some("zstd"));
    assert_eq!(header(&hz, "etag"), Some(etag.as_str()));
    assert_eq!(zstd::decode_all(&zst[..]).unwrap(), expected);
    let (_, hb, zst_b) = mock_form(&c, &id, "tok-b", Some("zstd"), None).await;
    assert_eq!(zst_b, zst, "deux pods, mêmes octets");
    assert_eq!(header(&hb, "etag"), Some(etag.as_str()));

    let (_, hg, gz) = mock_form(&c, &id, "tok-b", Some("gzip;q=1, zstd;q=0"), None).await;
    assert_eq!(header(&hg, "content-encoding"), Some("gzip"));
    let mut decoded = vec![];
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, expected);

    assert!(
        zst.len() * 20 <= full.len(),
        "forme de mock compressée : {} octets, snapshot complet : {}",
        zst.len(),
        full.len()
    );
    let relit = MockSnapshot::from_json(&plain, &id).unwrap();
    assert_eq!(relit.count, 2_000);
    assert!(relit.records.len() <= 30, "{}", relit.records.len());

    // Déjà reçue : 304, sans body.
    let (status, h304, body) = mock_form(&c, &id, "tok-b", Some("zstd"), Some(&etag)).await;
    assert_eq!(status, 304);
    assert!(body.is_empty());
    assert_eq!(header(&h304, "etag"), Some(etag.as_str()));

    // Sans jeton, ou snapshot inconnu.
    let r = client()
        .get(url(&c, &format!("{WL}/snapshots/{id}/mock")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let (status, _, _) = mock_form(&c, "0000000000000000", "tok-a", None, None).await;
    assert_eq!(status, 404);
}

/// Relevé de requêtes sans réponse tel qu'un sidecar l'annonce (spec §5.3).
fn unmatched_entry(
    method: &str,
    path: &str,
    reason: &str,
    count: u64,
    first: u64,
    last: u64,
) -> Value {
    json!({
        "authority": "api.saas.io:443", "method": method, "path": path, "reason": reason,
        "count": count, "first_ms": first, "last_ms": last,
    })
}

async fn beat(c: &Running, instance: &str, body: Value) -> reqwest::StatusCode {
    client()
        .post(url(c, &format!("{WL}/heartbeat")))
        .bearer_auth(token_of(instance))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ctl_36_unmatched_requests_reach_the_workload_views_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let c = controller(dir.path()).await;
    let a = json!({
        "total": 3,
        "entries": [
            unmatched_entry("POST", "/orders", "unmatched", 1, 2_000, 2_000),
            unmatched_entry("GET", "/users/{int}", "unmatched", 2, 1_000, 1_500),
        ],
    });
    let b = json!({
        "total": 5,
        "entries": [
            unmatched_entry("GET", "/users/{int}", "unmatched", 4, 500, 3_000),
            unmatched_entry("POST", "/upload", "too_large", 1, 2_500, 2_500),
        ],
    });
    let hb = |instance: &str, unmatched: Option<&Value>| {
        let mut v = json!({"instance": instance, "mode": "mock", "snapshot": null});
        if let Some(u) = unmatched {
            v["unmatched"] = u.clone();
        }
        v
    };
    assert_eq!(
        beat(&c, "checkout-a", hb("checkout-a", Some(&a))).await,
        200
    );
    // Champ inconnu (sidecar d'une version ultérieure) : ignoré.
    let mut future = hb("checkout-b", Some(&b));
    future["future"] = json!(1);
    assert_eq!(beat(&c, "checkout-b", future).await, 200);

    let list = get_json(&c, "/v1/workloads").await;
    let wl = &list[0];
    let pod = |wl: &Value, name: &str| {
        wl["pods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["instance"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(pod(wl, "checkout-a")["unmatched"], a);
    assert_eq!(pod(wl, "checkout-b")["unmatched"], b);
    assert_eq!(wl["unmatched"]["total"], 8, "{wl}");
    assert_eq!(
        wl["unmatched"]["entries"],
        json!([
            unmatched_entry("GET", "/users/{int}", "unmatched", 6, 500, 3_000),
            unmatched_entry("POST", "/upload", "too_large", 1, 2_500, 2_500),
            unmatched_entry("POST", "/orders", "unmatched", 1, 2_000, 2_000),
        ]),
        "{wl}"
    );

    let html = client()
        .get(ui(&c, "/ui/shop/checkout"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("Requêtes sans réponse du mock"), "{html}");
    assert!(html.contains("/users/{int}"), "{html}");
    assert!(html.contains("api.saas.io:443"), "{html}");

    let ctl = c.state().clone();
    let unmatched_status = || {
        let ctl = ctl.clone();
        async move {
            ctl.with_live("shop", "checkout", |w| {
                mimic_controller::reconcile::status_of(&w.desired, &ctl.summary(w), w.generation)
                    .unmatched
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(unmatched_status().await, 8);

    // Sidecar d'une version antérieure : pas de relevé, il compte pour 0.
    assert_eq!(beat(&c, "checkout-a", hb("checkout-a", None)).await, 200);
    let wl = &get_json(&c, "/v1/workloads").await[0];
    assert_eq!(wl["unmatched"]["total"], 5, "{wl}");
    assert_eq!(unmatched_status().await, 5);

    // Un relevé trop long est borné à 20 clés par pod.
    let entries: Vec<Value> = (0..30)
        .map(|i| unmatched_entry("GET", &format!("/p{i}"), "unmatched", 1, i, i))
        .collect();
    let long = json!({"total": 30, "entries": entries});
    assert_eq!(
        beat(&c, "checkout-a", hb("checkout-a", Some(&long))).await,
        200
    );
    let wl = &get_json(&c, "/v1/workloads").await[0];
    let kept = pod(wl, "checkout-a")["unmatched"]["entries"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(kept.len(), 20, "{wl}");
    assert_eq!(kept[0]["path"], "/p0");
}
