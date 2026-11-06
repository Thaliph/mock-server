//! SHR-07 and SHR-08: outbound documents, index, endpoint list and id grammar (spec §7.5, §7.6).

use proptest::prelude::*;
use serde_json::{Value, json};

use super::observed::Observation;
use super::openapi::{
    DESCRIPTION, DocId, document, endpoint_list, index, parse_document_id, parse_id,
};
use super::redact::Redactor;
use super::report::{Direction, Exchange, Report, Via, merge};
use super::scope::ScopeKey;
use super::tests_openapi::{LIMITS, assert_valid, errors, refs_ok};
use super::tests_openapi_ops::markdown_fields;

/// One outbound exchange.
#[derive(Clone, Debug)]
struct Out {
    authority: String,
    tls: bool,
    sni: Option<String>,
    dst: Option<&'static str>,
    method: &'static str,
    path: String,
    bearer: bool,
}

fn out(authority: &str, tls: bool, path: &str) -> Out {
    Out {
        authority: authority.into(),
        tls,
        sni: None,
        dst: None,
        method: "GET",
        path: path.into(),
        bearer: false,
    }
}

fn record_out(r: &mut Report, o: &Out) {
    let headers = [("authorization", "Bearer abc.def")];
    r.record_at(
        ScopeKey::Authority(o.authority.clone()),
        &Via {
            tls: o.tls,
            sni: o.sni.as_deref(),
            dst: o.dst.map(|d| d.parse().expect("socket address")),
        },
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Outbound,
            port: 0,
            method: o.method,
            path: &o.path,
            status: 200,
            head_ms: 7,
            host: None,
            grpc: false,
            observation: Observation {
                req_headers: if o.bearer { &headers } else { &[] },
                ..Observation::default()
            },
        },
    );
}

fn tunnel(r: &mut Report, authority: &str, kind: &str) {
    r.record_tunnel(&LIMITS, ScopeKey::Authority(authority.into()), kind);
}

fn inbound(r: &mut Report, port: u16, path: &str) {
    r.record(
        &LIMITS,
        &Redactor::default(),
        &Exchange {
            direction: Direction::Inbound,
            port,
            method: "GET",
            path,
            status: 200,
            head_ms: 10,
            host: Some("svc"),
            grpc: false,
            observation: Observation::default(),
        },
    );
}

fn inbound_only() -> Report {
    let mut r = Report::new(None);
    inbound(&mut r, 8080, "/users/42");
    inbound(&mut r, 9090, "/b");
    r
}

fn mixed() -> Report {
    let mut r = inbound_only();
    let mut api = out("api.example.com:443", true, "/v1/charges");
    api.sni = Some("api.example.com".into());
    api.dst = Some("93.184.216.34:443");
    api.bearer = true;
    record_out(&mut r, &api);
    record_out(&mut r, &out("api.example.com:443", false, "/v1/charges"));
    record_out(&mut r, &out("10.0.0.5:8080", false, "/health"));
    record_out(&mut r, &out("[2001:db8::1]:8443", true, "/v1/ping"));
    record_out(&mut r, &out("<redacted>:443", true, "/v1/secret"));
    record_out(&mut r, &out("api.example.com:443", true, "/v1/payments/42"));
    record_out(&mut r, &out("other.example:443", true, "/v1/payments/43"));
    tunnel(&mut r, "pay.example:443", "tls_excluded");
    tunnel(&mut r, "pay.example:443", "tls_excluded");
    tunnel(&mut r, "pay.example:443", "tcp");
    r
}

fn doc(r: &Report, id: &str) -> Value {
    document(r, "ns", "dep", 3, id).unwrap_or_else(|| panic!("no document {id}"))
}

// ---------------------------------------------------------------------------------------
// SHR-07

#[test]
fn shr_07_documents_of_each_scope_and_unchanged_inbound() {
    let r = mixed();
    let ids: Vec<String> = index(&r)["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .map(|d| d["id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(
        ids,
        [
            "inbound-8080",
            "inbound-9090",
            "outbound-10.0.0.5:8080",
            "outbound-<redacted>:443",
            "outbound-[2001:db8::1]:8443",
            "outbound-api.example.com:443",
            "outbound-other.example:443",
            "outbound-pay.example:443",
        ]
    );
    let plain = inbound_only();
    for id in ["inbound-8080", "inbound-9090"] {
        assert_eq!(doc(&r, id).to_string(), doc(&plain, id).to_string(), "{id}");
    }
    for id in &ids {
        let d = doc(&r, id);
        assert_valid(&d);
        assert!(refs_ok(&d), "{id}");
    }
}

#[test]
fn shr_07_title_authority_and_servers() {
    let r = mixed();
    let d = doc(&r, "outbound-api.example.com:443");
    assert_eq!(d["info"]["title"], "ns/dep outbound");
    assert_eq!(d["info"]["version"], "3");
    assert_eq!(d["info"]["description"], DESCRIPTION);
    assert_eq!(d["info"]["x-mimic-authority"], "api.example.com:443");
    assert_eq!(
        d["servers"],
        json!([{"url": "http://api.example.com:443"}, {"url": "https://api.example.com:443"}])
    );
    assert_eq!(
        d["x-mimic-origin"],
        json!({"sni": ["api.example.com"], "dst": ["93.184.216.34:443"]})
    );
    assert!(d.get("x-mimic-tunnels").is_none());

    let d = doc(&r, "outbound-10.0.0.5:8080");
    assert_eq!(d["servers"], json!([{"url": "http://10.0.0.5:8080"}]));
    assert!(d.get("x-mimic-origin").is_none());

    let d = doc(&r, "outbound-[2001:db8::1]:8443");
    assert_eq!(d["servers"], json!([{"url": "https://[2001:db8::1]:8443"}]));
    assert_eq!(d["info"]["x-mimic-authority"], "[2001:db8::1]:8443");

    let d = doc(&r, "outbound-<redacted>:443");
    assert!(d.get("servers").is_none());
    assert_eq!(d["info"]["x-mimic-authority"], "<redacted>:443");
    assert_eq!(d["info"]["title"], "ns/dep outbound");
}

#[test]
fn shr_07_tunnel_only_scope_has_empty_paths() {
    let r = mixed();
    let d = doc(&r, "outbound-pay.example:443");
    assert_eq!(d["paths"], json!({}));
    assert_eq!(d["x-mimic-tunnels"], json!({"tcp": 1, "tls_excluded": 2}));
    assert_eq!(d["info"]["x-mimic-authority"], "pay.example:443");
    assert!(d.get("servers").is_none());
    assert_valid(&d);
}

#[test]
fn shr_07_same_path_on_two_authorities_is_in_two_documents() {
    let r = mixed();
    for id in ["outbound-api.example.com:443", "outbound-other.example:443"] {
        let d = doc(&r, id);
        assert!(
            d["paths"]
                .as_object()
                .expect("paths")
                .contains_key("/v1/payments/{payments_id}")
                || d["paths"]
                    .as_object()
                    .expect("paths")
                    .contains_key("/v1/payments/43"),
            "{id}: {}",
            d["paths"]
        );
    }
}

#[test]
fn shr_07_bearer_is_http_bearer_and_no_hosts_extension() {
    let r = mixed();
    let d = doc(&r, "outbound-api.example.com:443");
    assert_eq!(
        d["components"]["securitySchemes"]["http_bearer"],
        json!({"type": "http", "scheme": "bearer"})
    );
    assert_eq!(
        d["paths"]["/v1/charges"]["get"]["security"],
        json!([{"http_bearer": []}])
    );
    assert!(!d.to_string().contains("x-mimic-hosts"));
}

#[test]
fn shr_07_unknown_documents_are_none() {
    let r = mixed();
    assert!(document(&r, "ns", "dep", 1, "outbound-absent.example:443").is_none());
    assert!(
        document(
            &inbound_only(),
            "ns",
            "dep",
            1,
            "outbound-api.example.com:443"
        )
        .is_none()
    );
    assert!(document(&r, "ns", "dep", 1, "outbound-80").is_none());
}

// ---------------------------------------------------------------------------------------
// SHR-08

#[test]
fn shr_08_index_entries() {
    let r = mixed();
    let list = index(&r);
    let docs = list["documents"].as_array().expect("documents");
    assert_eq!(docs.len(), 8);
    assert_eq!(
        docs[0],
        json!({"id": "inbound-8080", "direction": "inbound", "port": 8080, "endpoints": 1})
    );
    assert_eq!(
        docs[5],
        json!({"id": "outbound-api.example.com:443", "direction": "outbound",
               "authority": "api.example.com:443", "endpoints": 2, "tunnels": 0})
    );
    assert_eq!(
        docs[7],
        json!({"id": "outbound-pay.example:443", "direction": "outbound",
               "authority": "pay.example:443", "endpoints": 0, "tunnels": 3})
    );
    // Without outbound the index is byte-identical to the F6 one.
    assert_eq!(
        index(&inbound_only()).to_string(),
        r#"{"documents":[{"direction":"inbound","endpoints":1,"id":"inbound-8080","port":8080},{"direction":"inbound","endpoints":1,"id":"inbound-9090","port":9090}]}"#
    );
}

#[test]
fn shr_08_endpoint_list_inbound_first_then_outbound() {
    let r = mixed();
    let list = endpoint_list(&r);
    let rows = list["endpoints"].as_array().expect("endpoints");
    let documents: Vec<&str> = rows
        .iter()
        .map(|e| e["document"].as_str().expect("document"))
        .collect();
    assert_eq!(
        documents,
        [
            "inbound-8080",
            "inbound-9090",
            "outbound-10.0.0.5:8080",
            "outbound-<redacted>:443",
            "outbound-[2001:db8::1]:8443",
            "outbound-api.example.com:443",
            "outbound-api.example.com:443",
            "outbound-other.example:443",
        ]
    );
    let templates: Vec<&str> = rows[5..7]
        .iter()
        .map(|e| e["template"].as_str().expect("template"))
        .collect();
    let mut sorted = templates.clone();
    sorted.sort_unstable();
    assert_eq!(templates, sorted);
    let api = &rows[5];
    assert_eq!(api["direction"], "outbound");
    assert_eq!(api["authority"], "api.example.com:443");
    assert!(api.get("port").is_none());
    assert!(api.get("hosts").is_none());
    assert_eq!(api["statuses"], json!({"200": 2}));
    // The inbound rows are those of the inbound-only report.
    let plain = endpoint_list(&inbound_only());
    assert_eq!(rows[..2], plain["endpoints"].as_array().expect("rows")[..]);
}

#[test]
fn shr_08_id_grammar() {
    assert_eq!(parse_id("inbound-8080"), Some(8080));
    assert_eq!(parse_id("outbound-a.example:80"), None);
    assert_eq!(
        parse_document_id("inbound-8080"),
        Some(DocId::Inbound(8080))
    );
    for id in [
        "outbound-api.example.com:443",
        "outbound-10.0.0.5:8080",
        "outbound-[2001:db8::1]:8443",
        "outbound-<redacted>:443",
        "outbound-users.disc-22.svc:8080",
    ] {
        assert_eq!(
            parse_document_id(id),
            Some(DocId::Outbound(id["outbound-".len()..].to_string())),
            "{id}"
        );
    }
    for id in [
        "outbound-",
        "outbound-a.example",
        "outbound-A.example:80",
        "outbound-a.example:080",
        "outbound-a/b:80",
        "outbound-80",
        "inbound-80 ",
        "inbound-0",
        "inbound-08080",
        "foo",
        "",
        "Outbound-a.example:80",
    ] {
        assert_eq!(parse_document_id(id), None, "{id}");
    }
}

fn arb_key() -> impl Strategy<Value = String> {
    prop_oneof![
        ".{0,40}",
        "[a-z0-9._-]{0,12}:[0-9]{0,6}",
        "\\[[0-9a-f:]{0,20}\\]:[0-9]{1,5}",
        "[0-9]{1,6}",
        prop::sample::select(vec![
            "<redacted>:443".to_string(),
            "a.example:80".to_string(),
            "A.example:80".to_string(),
            "a.example:080".to_string(),
            "[2001:db8::1]:8443".to_string(),
            "[2001:0db8::1]:8443".to_string(),
        ]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn shr_08_id_grammar_is_the_scope_key_grammar(key in arb_key()) {
        let outbound = parse_document_id(&format!("outbound-{key}"));
        let inbound = parse_document_id(&format!("inbound-{key}"));
        match ScopeKey::parse(&key) {
            Some(ScopeKey::Authority(a)) => {
                prop_assert_eq!(outbound, Some(DocId::Outbound(a)));
                prop_assert_eq!(inbound, None);
            }
            Some(ScopeKey::Port(p)) => {
                prop_assert_eq!(outbound, None);
                prop_assert_eq!(inbound, Some(DocId::Inbound(p)));
            }
            None => {
                prop_assert_eq!(outbound, None);
                prop_assert_eq!(inbound, None);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// UI-01 and UI-06 with outbound scopes

const CANARY: &str = "canaryfixed00";

fn arb_out() -> impl Strategy<Value = Out> {
    let hosts = vec![
        format!("{CANARY}.example:443"),
        "api.example.com:443".to_string(),
        "10.0.0.5:8080".to_string(),
        "[2001:db8::1]:8443".to_string(),
        "<redacted>:443".to_string(),
    ];
    (
        prop::sample::select(hosts),
        any::<bool>(),
        prop::option::of(Just(CANARY.to_string())),
        prop::sample::select(vec![None, Some("10.0.0.5:8080")]),
        prop::sample::select(vec!["GET", "POST", "PROPFIND"]),
        prop::sample::select(vec!["/", "/v1/a", "/v1/a/1", "/v1/a/2", "/x/{y}"]),
        any::<bool>(),
    )
        .prop_map(|(authority, tls, sni, dst, method, path, bearer)| Out {
            authority,
            tls,
            sni,
            dst,
            method,
            path: path.into(),
            bearer,
        })
}

fn render(r: &Report) -> Vec<String> {
    let mut all = vec![index(r).to_string(), endpoint_list(r).to_string()];
    for d in index(r)["documents"].as_array().expect("documents") {
        all.push(doc(r, d["id"].as_str().expect("id")).to_string());
    }
    all
}

fn built(outs: &[Out], tunnels: &[(String, &str)], chunk: usize, reverse: bool) -> Report {
    let mut parts: Vec<Report> = outs
        .chunks(chunk)
        .map(|c| {
            let mut r = Report::new(None);
            c.iter().for_each(|o| record_out(&mut r, o));
            r
        })
        .collect();
    let mut t = Report::new(None);
    tunnels.iter().for_each(|(a, k)| tunnel(&mut t, a, k));
    parts.push(t);
    if reverse {
        parts.reverse();
    }
    parts
        .into_iter()
        .reduce(|a, b| merge(a, b, &LIMITS))
        .unwrap_or_else(|| Report::new(None))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn shr_07_deterministic_valid_and_no_recorded_data_in_markdown_fields(
        canary in "canary[a-z0-9]{8,20}",
        outs in prop::collection::vec(arb_out(), 0..8),
        tunnels in prop::collection::vec(
            (
                prop::sample::select(vec![
                    format!("{CANARY}.example:443"),
                    "t.example:443".to_string(),
                    "<redacted>:80".to_string(),
                ]),
                prop::sample::select(vec!["tcp", "connect", "tls_no_ca"]),
            ),
            0..4,
        ),
        chunk in 1usize..4,
    ) {
        // A tunnel-only scope named after a random canary, next to the fixed one.
        let mut tunnels = tunnels;
        tunnels.push((format!("{canary}.example:443"), "tcp"));
        let whole = built(&outs, &tunnels, outs.len().max(1), false);
        let want = render(&whole);
        prop_assert_eq!(&want, &render(&whole));
        prop_assert_eq!(&want, &render(&built(&outs, &tunnels, chunk, false)));
        prop_assert_eq!(&want, &render(&built(&outs, &tunnels, chunk, true)));
        for id in index(&whole)["documents"].as_array().unwrap() {
            let d = doc(&whole, id["id"].as_str().unwrap());
            let problems = errors(&d);
            prop_assert!(problems.is_empty(), "{:?}\n{}", problems, d);
            prop_assert!(refs_ok(&d), "{}", d);
            prop_assert_eq!(&d["openapi"], "3.1.1");
            prop_assert_eq!(&d["info"]["title"], "ns/dep outbound");
            let mut fields = Vec::new();
            markdown_fields(&d, &mut fields);
            for field in &fields {
                prop_assert!(!field.contains(&canary) && !field.contains(CANARY), "{}", field);
            }
            prop_assert_eq!(d["info"]["description"].as_str(), Some(DESCRIPTION));
        }
    }
}
