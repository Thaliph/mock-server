//! Interception TLS sortante : PRX-06, PRX-07 et détection de l'épinglage (spec §4.2).

mod common;

use std::time::Duration;

use common::*;
use mimic_proxy::Running;
use mimic_proxy::config::{Config, Mode};
use serde_json::Value;

struct Setup {
    proxy: Running,
    saas: Fake,
    saas_pem: String,
    ca_dir: tempfile::TempDir,
}

async fn setup(intercept: &[&str], mode: Mode) -> Setup {
    let (saas, saas_pem) = fake_https_server().await;
    let ca_dir = tempfile::tempdir().unwrap();
    let upstream_ca = ca_dir.path().join("upstream.pem");
    std::fs::write(&upstream_ca, &saas_pem).unwrap();
    let mut c: Config = config(None, mode);
    c.tls.intercept = intercept.iter().map(|s| s.to_string()).collect();
    c.tls.ca_dir = Some(ca_dir.path().join("out"));
    c.tls.upstream_ca_file = Some(upstream_ca);
    let proxy = common::proxy(c).await;
    Setup {
        proxy,
        saas,
        saas_pem,
        ca_dir,
    }
}

impl Setup {
    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.saas.addr.port())
    }

    /// Application configurée avec `HTTPS_PROXY` et le bundle publié par le sidecar.
    fn app(&self) -> reqwest::Client {
        let ca = std::fs::read(self.ca_dir.path().join("out/ca.crt")).unwrap();
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{}", self.proxy.outbound_addr())).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
            .build()
            .unwrap()
    }

    /// Application qui ne fait confiance qu'au vrai certificat de la dépendance.
    fn pinned_app(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{}", self.proxy.outbound_addr())).unwrap())
            .tls_certs_only([reqwest::Certificate::from_pem(self.saas_pem.as_bytes()).unwrap()])
            .build()
            .unwrap()
    }
}

#[tokio::test]
async fn ca_files_are_published_for_the_app() {
    let s = setup(&["localhost"], Mode::Record).await;
    let ca = std::fs::read_to_string(s.ca_dir.path().join("out/ca.crt")).unwrap();
    let bundle = std::fs::read_to_string(s.ca_dir.path().join("out/ca-bundle.crt")).unwrap();
    assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(bundle.ends_with(&ca));
}

#[tokio::test]
async fn prx_06_intercepted_https_is_recorded_then_mocked() {
    let s = setup(&["localhost"], Mode::Record).await;
    let app = s.app();
    let r = app.get(s.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "user3");

    let e = wait_exchanges(&s.proxy, "direction=outbound", 1)
        .await
        .remove(0);
    assert_eq!(e["scheme"], "https");
    assert_eq!(e["authority"], format!("localhost:{}", s.saas.addr.port()));
    assert_eq!(e["path"], "/users/3");

    let hits = s.saas.hits();
    s.saas.stop();
    set_mode(&s.proxy, "mock").await;
    let r = app.get(s.url("/users/3")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().contains_key("x-mimic-match"));
    assert_eq!(r.json::<Value>().await.unwrap()["id"], 3);
    assert_eq!(s.saas.hits(), hits);
}

#[tokio::test]
async fn prx_07_other_hosts_are_tunnelled_blind() {
    let s = setup(&["api.other.example"], Mode::Record).await;
    let r = s.pinned_app().get(s.url("/users/1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exchanges(&s.proxy, "direction=outbound").await.is_empty());
}

#[tokio::test]
async fn pinned_client_rejection_is_counted() {
    let s = setup(&["localhost"], Mode::Record).await;
    assert!(s.pinned_app().get(s.url("/users/1")).send().await.is_err());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let m = metrics(&s.proxy).await;
        if m.contains("mimic_tls_handshake_failures_total{") {
            assert!(m.contains(r#"reason="client_rejected""#), "{m}");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{m}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
