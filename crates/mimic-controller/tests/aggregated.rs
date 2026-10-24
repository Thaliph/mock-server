//! Critères CTL-47 et CTL-48 de la spec (§9.2c) : API agrégée de lecture, authentifiée par
//! le certificat de front proxy de l'API server et autorisée par `SubjectAccessReview`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use mimic_controller::aggregated::{Access, Authorizer, PREFIX};
use mimic_controller::config::{
    AggregatedConfig, AuthConfig, ControllerConfig, Identity, TlsFiles,
};
use mimic_controller::{Running, start_authorized};
use serde_json::{Value, json};

const ALLOW: u8 = 0;
const DENY: u8 = 1;
const FAIL: u8 = 2;

/// Autorisation simulée : garde chaque accès demandé, répond selon `mode`.
#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Access>>,
    mode: AtomicU8,
}

impl Authorizer for Fake {
    fn allowed<'a>(&'a self, access: &'a Access) -> BoxFuture<'a, Result<bool, String>> {
        self.seen.lock().unwrap().push(access.clone());
        let mode = self.mode.load(Ordering::SeqCst);
        Box::pin(async move {
            match mode {
                ALLOW => Ok(true),
                DENY => Ok(false),
                _ => Err("API server injoignable".into()),
            }
        })
    }
}

/// Certificats de test : CA de front proxy, certificat client, serveur `localhost`.
struct Pki {
    _dir: tempfile::TempDir,
    server_pem: String,
    server: TlsFiles,
    ca_file: std::path::PathBuf,
    ca: Ca,
    other_ca: Ca,
}

/// CA de test : son certificat, et de quoi signer.
struct Ca {
    cert: rcgen::Certificate,
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
}

fn ca(name: &str) -> Ca {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    Ca {
        cert: params.self_signed(&key).unwrap(),
        params,
        key,
    }
}

/// Certificat client au nom commun `cn`, signé par `issuer` : PEM certificat puis clé.
fn client(cn: &str, issuer: &Ca) -> reqwest::Identity {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, cn);
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let issuer = rcgen::Issuer::from_params(&issuer.params, &issuer.key);
    let cert = params.signed_by(&key, &issuer).unwrap();
    reqwest::Identity::from_pem(format!("{}{}", cert.pem(), key.serialize_pem()).as_bytes())
        .unwrap()
}

fn pki() -> Pki {
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let server = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let files = TlsFiles::new(dir.path().join("tls.crt"), dir.path().join("tls.key"));
    std::fs::write(&files.cert_file, server.pem()).unwrap();
    std::fs::write(&files.key_file, key.serialize_pem()).unwrap();
    let front = ca("front-proxy-ca");
    let ca_file = dir.path().join("front-proxy-ca.crt");
    std::fs::write(&ca_file, front.cert.pem()).unwrap();
    Pki {
        server_pem: server.pem(),
        server: files,
        ca_file,
        ca: front,
        other_ca: ca("autre-ca"),
        _dir: dir,
    }
}

fn identity(ns: &str, pod: &str, workload: &str) -> Identity {
    Identity {
        namespace: ns.into(),
        pod: pod.into(),
        workload: workload.into(),
        ..Identity::default()
    }
}

async fn controller(pki: &Pki, store: &std::path::Path, authorizer: Arc<Fake>) -> Running {
    start_authorized(
        ControllerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ui_listen: "127.0.0.1:0".parse().unwrap(),
            store: format!("file://{}", store.display()),
            tls: Some(pki.server.clone()),
            auth: AuthConfig::Static {
                tokens: BTreeMap::from([
                    (
                        "tok-a".to_string(),
                        identity("shop", "checkout-a", "checkout"),
                    ),
                    (
                        "tok-bank".to_string(),
                        identity("bank", "ledger-a", "ledger"),
                    ),
                ]),
            },
            aggregated: Some(AggregatedConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                client_ca_file: Some(pki.ca_file.clone()),
                allowed_names: vec!["front-proxy-client".into()],
            }),
            ..ControllerConfig::default()
        },
        authorizer,
    )
    .await
    .unwrap()
}

/// Client de l'API agrégée, tel que l'API server : certificat de front proxy facultatif.
fn front_proxy(pki: &Pki, identity: Option<reqwest::Identity>) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .tls_certs_only([reqwest::Certificate::from_pem(pki.server_pem.as_bytes()).unwrap()]);
    if let Some(i) = identity {
        b = b.identity(i);
    }
    b.build().unwrap()
}

fn aggregated(c: &Running, path: &str) -> String {
    format!(
        "https://localhost:{}{PREFIX}{path}",
        c.aggregated_addr().unwrap().port()
    )
}

/// Échange minimal au format du store.
fn exchange(id: &str, direction: &str, path: &str) -> Value {
    json!({
        "id": id, "direction": direction, "started_at_ms": 1, "scheme": "http",
        "authority": if direction == "outbound" { "api.saas.io:443" } else { "checkout:8080" },
        "method": "GET", "path": path, "query": [],
        "req_headers": [], "req_body": {"kind": "empty"}, "status": 200,
        "resp_headers": [{"name": "content-type", "value": "application/json"}],
        "resp_body": {"kind": "captured", "data": "eyJpZCI6N30="},
        "ttfb_us": 1000, "total_us": 2000, "trace_id": null
    })
}

async fn push(c: &Running, ns: &str, wl: &str, instance: &str, token: &str, ex: &[Value]) {
    let body: String = ex.iter().map(|e| format!("{e}\n")).collect();
    let r = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .post(format!(
            "https://localhost:{}/v1/workloads/{ns}/{wl}/segments?instance={instance}",
            c.api_addr().port()
        ))
        .bearer_auth(token)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
}

async fn ui(c: &Running, path: &str) -> String {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}{path}", c.ui_addr()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

fn get(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("x-remote-user", "alice")
        .header("x-remote-group", "dev")
        .header("x-remote-group", "system:authenticated")
        .header("x-remote-extra-scopes", "a")
}

fn access(verb: &str, ns: Option<&str>, name: Option<&str>, sub: Option<&str>) -> Access {
    Access {
        user: "alice".into(),
        groups: vec!["dev".into(), "system:authenticated".into()],
        extra: BTreeMap::from([("scopes".to_string(), vec!["a".to_string()])]),
        verb: verb.into(),
        namespace: ns.map(Into::into),
        name: name.map(Into::into),
        subresource: sub.map(Into::into),
    }
}

#[tokio::test]
async fn ctl_47_reads_are_authorized_per_namespace_and_workload() {
    let pki = pki();
    let store = tempfile::tempdir().unwrap();
    let authz = Arc::new(Fake::default());
    let c = controller(&pki, store.path(), authz.clone()).await;
    push(
        &c,
        "shop",
        "checkout",
        "checkout-a",
        "tok-a",
        &[
            exchange("1", "inbound", "/a"),
            exchange("2", "outbound", "/b"),
        ],
    )
    .await;
    push(
        &c,
        "bank",
        "ledger",
        "ledger-a",
        "tok-bank",
        &[exchange("3", "inbound", "/c")],
    )
    .await;
    let http = front_proxy(&pki, Some(client("front-proxy-client", &pki.ca)));

    // Découverte : sans autorisation demandée.
    let root: Value = get(&http, &aggregated(&c, ""))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(root["kind"], "APIResourceList");
    assert_eq!(root["groupVersion"], "data.mimic.platform.io/v1alpha1");
    assert!(authz.seen.lock().unwrap().is_empty());

    // Chaque route : l'accès du tableau, et le corps de la route de l'interface.
    let wl = "/namespaces/shop/workloads/checkout";
    let ui_wl = "/v1/workloads/shop/checkout";
    for (path, ui_path, sub) in [
        ("/operations", "/operations", "operations"),
        ("/openapi/inbound", "/openapi/inbound", "openapi"),
        (
            "/requests?direction=outbound&format=jsonl&limit=10",
            "/requests?direction=outbound&format=jsonl&limit=10",
            "requests",
        ),
        ("/snapshots", "/snapshots", "snapshots"),
    ] {
        let r = get(&http, &aggregated(&c, &format!("{wl}{path}")))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert_eq!(
            r.text().await.unwrap(),
            ui(&c, &format!("{ui_wl}{ui_path}")).await,
            "{path}"
        );
        assert_eq!(
            authz.seen.lock().unwrap().last().unwrap(),
            &access("get", Some("shop"), Some("checkout"), Some(sub)),
            "{path}"
        );
    }

    // Liste d'un namespace : ses seuls workloads ; liste du cluster : `list` sans namespace.
    let list: Value = get(&http, &aggregated(&c, "/namespaces/shop/workloads"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let namespaces: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["namespace"].as_str().unwrap())
        .collect();
    assert_eq!(namespaces, ["shop"]);
    assert_eq!(
        authz.seen.lock().unwrap().last().unwrap(),
        &access("list", Some("shop"), None, None)
    );
    let all: Value = get(&http, &aggregated(&c, "/workloads"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(
        authz.seen.lock().unwrap().last().unwrap(),
        &access("list", None, None, None)
    );

    // Décision gardée : la même lecture ne redemande rien.
    let before = authz.seen.lock().unwrap().len();
    let r = get(&http, &aggregated(&c, &format!("{wl}/operations")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(authz.seen.lock().unwrap().len(), before);

    // Refus, puis autorisation indisponible, sur des accès pas encore décidés.
    authz.mode.store(DENY, Ordering::SeqCst);
    let r = get(
        &http,
        &aggregated(&c, "/namespaces/bank/workloads/ledger/operations"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 403);
    let status: Value = r.json().await.unwrap();
    assert_eq!(status["kind"], "Status");
    assert_eq!(status["reason"], "Forbidden");
    assert!(
        status["message"].as_str().unwrap().contains("alice"),
        "{status}"
    );
    authz.mode.store(FAIL, Ordering::SeqCst);
    let r = get(
        &http,
        &aggregated(&c, "/namespaces/bank/workloads/ledger/requests"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 503);

    // Lecture seule.
    authz.mode.store(ALLOW, Ordering::SeqCst);
    let r = http
        .post(aggregated(&c, &format!("{wl}/snapshots")))
        .header("x-remote-user", "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 405);
}

#[tokio::test]
async fn ctl_48_only_the_front_proxy_may_speak_for_a_user() {
    let pki = pki();
    let store = tempfile::tempdir().unwrap();
    let authz = Arc::new(Fake::default());
    let c = controller(&pki, store.path(), authz.clone()).await;
    let url = aggregated(&c, "/namespaces/shop/workloads/checkout/operations");

    for (case, identity) in [
        ("sans certificat", None),
        (
            "autre CA",
            Some(client("front-proxy-client", &pki.other_ca)),
        ),
        ("nom refusé", Some(client("intrus", &pki.ca))),
    ] {
        let r = get(&front_proxy(&pki, identity), &url).send().await;
        assert!(r.is_err(), "{case} : {r:?}");
    }
    assert!(authz.seen.lock().unwrap().is_empty());

    // Sur l'API des sidecars, l'en-tête ne vaut rien : un jeton est exigé.
    let r = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(format!(
            "https://localhost:{}/v1/workloads/shop/checkout/segments",
            c.api_addr().port()
        ))
        .header("x-remote-user", "system:admin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // L'API agrégée exige TLS.
    let config = ControllerConfig {
        aggregated: Some(AggregatedConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            client_ca_file: Some(pki.ca_file.clone()),
            allowed_names: vec![],
        }),
        ..ControllerConfig::default()
    };
    assert!(config.validate().unwrap_err().contains("aggregated"));
}
