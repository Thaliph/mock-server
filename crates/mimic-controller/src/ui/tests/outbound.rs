//! SHR-13 (read routes with outbound) and SHR-18 (controller memory), spec §7.5, §7.6, §12.9.

use mimic_core::discovery::openapi;
use mimic_core::discovery::report::Via;
use mimic_core::discovery::scope::ScopeKey;

use super::*;

const LOCAL: &str = "users.disc-22.svc:8080";
const V6: &str = "[2001:db8::1]:8443";
const REDACTED: &str = "<redacted>:443";

fn record_outbound(report: &mut Report, authority: &str, path: &str, times: usize) {
    for _ in 0..times {
        report.record_at(
            ScopeKey::Authority(authority.to_string()),
            &Via {
                tls: true,
                sni: Some("api.example.com"),
                dst: None,
            },
            &LIMITS,
            &Redactor::default(),
            &Exchange {
                direction: Direction::Outbound,
                port: 443,
                method: "GET",
                path,
                status: 200,
                head_ms: 3,
                host: None,
                grpc: false,
                observation: Observation::default(),
            },
        );
    }
}

/// One v2 push: an inbound `/a` and two outbound exchanges per `(authority, path)`.
async fn seed_outbound(store: &Arc<Store>, ns: &str, dep: &str, outbound: &[(&str, &str)]) {
    let mut push = push_of("front-1", 1, &[("/a", 200)]);
    let mut report = Report::from_json(&push.json).expect("reads");
    for (authority, path) in outbound {
        record_outbound(&mut report, authority, path, 2);
    }
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / DAY);
    report.stamp(today);
    push.json = report.to_json().expect("serialises");
    store.push(ns, dep, push).await.expect("push applied");
}

/// The id of an authority with every reserved character percent-encoded, as `mimic.js` sends it.
fn encoded(authority: &str) -> String {
    let mut out = String::from("outbound-");
    for b in authority.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn seeded(prefix: &str) -> (Arc<Store>, Router, String) {
    let store = store(&valkey_url());
    let (app, _) = app(&store);
    let ns = unique(prefix);
    seed_outbound(
        &store,
        &ns,
        "front",
        &[(LOCAL, "/users/1"), (V6, "/v6"), (REDACTED, "/r")],
    )
    .await;
    (store, app, ns)
}

fn offline_app() -> Router {
    app(&store("redis://127.0.0.1:1")).0
}

fn text_of(answer: &Full) -> String {
    String::from_utf8(answer.body.clone()).expect("UTF-8")
}

fn document_ids(answer: &Full) -> Vec<String> {
    let value: Value = serde_json::from_slice(&answer.body).unwrap();
    value["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn shr_13_report_is_the_union_with_version_2_and_the_stored_bytes_without_outbound() {
    let (store, app, ns) = seeded("shr13a").await;
    let answer = fetch(&app, &format!("/v1/workloads/{ns}/front/report")).await;
    assert_eq!(answer.status, 200);
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["version"], 2);
    let report = Report::from_json(std::str::from_utf8(&answer.body).unwrap()).unwrap();
    assert_eq!(report.endpoints().len(), 4);
    let plain = unique("shr13b");
    seed(&store, &plain, "front", &["front-1"]).await;
    let k = keys(&plain, "front").unwrap();
    let want: Vec<u8> = redis::cmd("GET")
        .arg(&k.report)
        .query_async(&mut connection().await)
        .await
        .unwrap();
    let answer = fetch(&app, &format!("/v1/workloads/{plain}/front/report")).await;
    assert_eq!(answer.body, want);
}

#[tokio::test]
async fn shr_13_the_routes_list_and_render_the_outbound_documents() {
    let (_, app, ns) = seeded("shr13c").await;
    let base = format!("/v1/workloads/{ns}/front");
    let union = fetch(&app, &format!("{base}/report")).await;
    let report = Report::from_json(std::str::from_utf8(&union.body).unwrap()).unwrap();
    let version = stored(&ns, "front").await.1;
    let index = fetch(&app, &format!("{base}/openapi")).await;
    assert_eq!(index.status, 200);
    assert_eq!(index.body, bytes_of(&openapi::index(&report)));
    let ids = document_ids(&index);
    for authority in [LOCAL, V6, REDACTED] {
        assert!(ids.contains(&format!("outbound-{authority}")), "{ids:?}");
    }
    let list = fetch(&app, &format!("{base}/endpoints")).await;
    assert_eq!(list.body, bytes_of(&openapi::endpoint_list(&report)));
    for authority in [LOCAL, V6, REDACTED] {
        let path = encoded(authority);
        let answer = fetch(&app, &format!("{base}/openapi/{path}")).await;
        assert_eq!(answer.status, 200, "{path}");
        assert_eq!(answer.headers["content-type"], "application/json");
        let id = format!("outbound-{authority}");
        let want = openapi::document(&report, &ns, "front", version, &id).unwrap();
        assert_eq!(answer.body, bytes_of(&want), "{path}");
    }
    // The unreserved form of a plain authority is the same document.
    let plain = fetch(&app, &format!("{base}/openapi/outbound-{LOCAL}")).await;
    assert_eq!(plain.status, 200);
}

#[tokio::test]
async fn shr_13_bad_encodings_and_ids_are_400_and_an_absent_authority_404() {
    let (_, app, ns) = seeded("shr13d").await;
    let base = format!("/v1/workloads/{ns}/front/openapi");
    for id in [
        "outbound-a%2Fb:80",
        "outbound-a%2fb%3A80",
        "outbound-%ZZ",
        "outbound-%FF%FE%3A80",
        "outbound-",
        "outbound-a.example:0",
        "outbound-a.example:080",
        "outbound-a.example:65536",
        "outbound-80",
        "outbound-%5B::1%5D",
    ] {
        let answer = fetch(&app, &format!("{base}/{id}")).await;
        assert_eq!(answer.status, 400, "{id}");
    }
    let absent = fetch(&app, &format!("{base}/outbound-absent.example:443")).await;
    assert_eq!(absent.status, 404);
}

#[tokio::test]
async fn shr_13_the_page_serves_encoded_outbound_ids_and_refuses_bad_ones() {
    let app = offline_app();
    for authority in [LOCAL, V6, REDACTED] {
        let path = format!("/ui/workloads/shop/front/openapi/{}", encoded(authority));
        let page = fetch(&app, &path).await;
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(page.headers["content-type"], "text/html; charset=utf-8");
    }
    for id in [
        "outbound-a%2Fb:80",
        "outbound-%ZZ",
        "outbound-%FF",
        "outbound-a.example:0",
    ] {
        let path = format!("/ui/workloads/shop/front/openapi/{id}");
        assert_eq!(fetch(&app, &path).await.status, 400, "{path}");
    }
}

#[tokio::test]
async fn shr_13_ui_07_and_ui_08_hold_on_the_outbound_routes() {
    let (_, app, ns) = seeded("shr13e").await;
    let path = format!("/v1/workloads/{ns}/front/openapi/{}", encoded(LOCAL));
    let local = Some("localhost:8080");
    let ok = send(&app, Method::GET, &path, "127.0.0.1", local).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.headers["cache-control"], "no-store");
    assert_eq!(ok.headers["x-content-type-options"], "nosniff");
    assert!(ok.headers.contains_key("content-security-policy"));
    assert!(!ok.headers.contains_key("access-control-allow-origin"));
    let head = send(&app, Method::HEAD, &path, "127.0.0.1", local).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    let post = send(&app, Method::POST, &path, "127.0.0.1", local).await;
    assert_eq!(post.status, 405);
    assert_eq!(post.headers["allow"], "GET, HEAD");
    let remote = send(&app, Method::GET, &path, "10.1.2.3", local).await;
    assert_eq!(remote.status, 403);
    let host = send(&app, Method::GET, &path, "127.0.0.1", Some("evil.example")).await;
    assert_eq!(host.status, 403);
    let (down_app, _) = self::app(&store("redis://127.0.0.1:1"));
    assert_eq!(fetch(&down_app, &path).await.status, 503);
}

#[tokio::test]
async fn shr_13_mimic_js_shows_the_authority_and_encodes_the_ids() {
    let app = offline_app();
    let js = text_of(&fetch(&app, "/ui/assets/mimic.js").await);
    assert!(js.contains("encodeURIComponent"));
    assert!(js.contains("\"Authority\""), "no authority column");
    assert!(
        js.contains("e.authority"),
        "authority is not read from the list"
    );
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
    ] {
        assert!(!js.contains(forbidden), "{forbidden}");
    }
}

#[tokio::test]
async fn exp_09_every_endpoint_row_names_a_document_the_tab_can_open() {
    let (_, app, ns) = seeded("exp09").await;
    let base = format!("/v1/workloads/{ns}/front");
    let list: Value = serde_json::from_slice(&fetch(&app, &format!("{base}/endpoints")).await.body)
        .expect("JSON");
    let rows = list["endpoints"].as_array().expect("rows");
    assert_eq!(rows.len(), 4, "{list}");
    let mut seen = Vec::new();
    for row in rows {
        for member in ["direction", "method", "template", "exchanges", "statuses"] {
            assert!(!row[member].is_null(), "{member} in {row}");
        }
        let document = row["document"].as_str().expect("document");
        match row["direction"].as_str().unwrap() {
            "inbound" => {
                assert_eq!(document, format!("inbound-{}", row["port"]), "{row}");
            }
            "outbound" => {
                let authority = row["authority"].as_str().expect("authority");
                assert_eq!(document, format!("outbound-{authority}"), "{row}");
            }
            other => panic!("direction {other}"),
        }
        let path = if let Some(authority) = document.strip_prefix("outbound-") {
            encoded(authority)
        } else {
            document.to_string()
        };
        let answer = fetch(&app, &format!("{base}/openapi/{path}")).await;
        assert_eq!(answer.status, 200, "{path}");
        seen.push(document.to_string());
    }
    assert!(seen.iter().any(|d| d.starts_with("inbound-")), "{seen:?}");
    for authority in [LOCAL, V6, REDACTED] {
        assert!(seen.contains(&format!("outbound-{authority}")), "{seen:?}");
    }
    let page = fetch(&app, "/ui/endpoints").await;
    assert_eq!(page.status, 200);
    assert!(!text_of(&page).contains(&ns));
}
