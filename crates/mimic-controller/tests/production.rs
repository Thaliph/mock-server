//! Contrôleur en production (spec §9.2b) : certificat relu à chaud (CTL-29), authentification
//! avant la lecture du corps (CTL-31), `/metrics` sur le port de l'API (CTL-32).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use mimic_controller::config::{AuthConfig, ControllerConfig, Identity, TlsFiles};
use mimic_controller::{Running, start};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const WL: &str = "/v1/workloads/shop/checkout";

fn config(dir: &std::path::Path) -> ControllerConfig {
    let identity = |pod: &str, workload: &str| Identity {
        namespace: "shop".into(),
        pod: pod.into(),
        workload: workload.into(),
        ..Identity::default()
    };
    let tokens = BTreeMap::from([
        ("tok-a".to_string(), identity("checkout-a", "checkout")),
        ("tok-web".to_string(), identity("web-a", "web")),
    ]);
    ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ui_listen: "127.0.0.1:0".parse().unwrap(),
        store: format!("file://{}", dir.join("store").display()),
        auth: AuthConfig::Static { tokens },
        ..ControllerConfig::default()
    }
}

/// Certificat auto-signé pour `localhost` : (certificat PEM, clé PEM).
fn self_signed() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (cert.pem(), key.serialize_pem())
}

fn client_trusting(cert: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .tls_certs_only([reqwest::Certificate::from_pem(cert.as_bytes()).unwrap()])
        .build()
        .unwrap()
}

#[tokio::test]
async fn ctl_29_renewed_certificate_is_served_without_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = self_signed();
    let mut tls = TlsFiles::new(dir.path().join("tls.crt"), dir.path().join("tls.key"));
    tls.reload_interval = Duration::from_millis(100);
    std::fs::write(&tls.cert_file, &cert).unwrap();
    std::fs::write(&tls.key_file, key).unwrap();
    let c = start(ControllerConfig {
        tls: Some(tls.clone()),
        ..config(dir.path())
    })
    .await
    .unwrap();
    let healthz = format!("https://localhost:{}/healthz", c.api_addr().port());
    assert!(client_trusting(&cert).get(&healthz).send().await.is_ok());

    let (renewed, key) = self_signed();
    std::fs::write(&tls.cert_file, &renewed).unwrap();
    std::fs::write(&tls.key_file, key).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client_trusting(&renewed)
        .get(&healthz)
        .send()
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "certificat renouvelé jamais servi"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(&tls.cert_file, "illisible").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        client_trusting(&renewed).get(&healthz).send().await.is_ok(),
        "l'ancien certificat reste servi"
    );
    c.shutdown().await;
}

/// Envoie les en-têtes (et `body`), puis lit la ligne de statut ; `None` si rien n'arrive
/// en 2 s.
async fn status_of(c: &Running, head: &str, body: &[u8]) -> Option<String> {
    let mut s = TcpStream::connect(c.api_addr()).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    text.lines().next().map(String::from)
}

fn post(path: &str, token: Option<&str>, length: usize) -> String {
    let auth = token.map_or(String::new(), |t| format!("authorization: Bearer {t}\r\n"));
    format!(
        "POST {path} HTTP/1.1\r\nhost: x\r\n{auth}content-type: application/json\r\ncontent-length: {length}\r\n\r\n"
    )
}

#[tokio::test]
async fn ctl_31_callers_are_authenticated_before_the_body_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(config(dir.path())).await.unwrap();
    let segments = format!("{WL}/segments?instance=checkout-a");
    let heartbeat = format!("{WL}/heartbeat");
    let big = 64 * 1024 * 1024;
    for path in [&segments, &heartbeat] {
        let line = status_of(&c, &post(path, None, big), b"").await;
        assert_eq!(line.as_deref(), Some("HTTP/1.1 401 Unauthorized"), "{path}");
        let line = status_of(&c, &post(path, Some("tok-web"), big), b"").await;
        assert_eq!(line.as_deref(), Some("HTTP/1.1 403 Forbidden"), "{path}");
    }
    // Battement plus gros que 64 KiB : refusé, même authentifié.
    let body = vec![b' '; 65 * 1024];
    let line = status_of(&c, &post(&heartbeat, Some("tok-a"), body.len()), &body).await;
    assert_eq!(line.as_deref(), Some("HTTP/1.1 413 Payload Too Large"));
    // Un battement ordinaire passe toujours ; l'instance du corps est vérifiée.
    let ok = br#"{"instance":"checkout-a","mode":"record","snapshot":null}"#;
    let line = status_of(&c, &post(&heartbeat, Some("tok-a"), ok.len()), ok).await;
    assert_eq!(line.as_deref(), Some("HTTP/1.1 200 OK"));
    let other = br#"{"instance":"checkout-b","mode":"record","snapshot":null}"#;
    let line = status_of(&c, &post(&heartbeat, Some("tok-a"), other.len()), other).await;
    assert_eq!(line.as_deref(), Some("HTTP/1.1 403 Forbidden"));
    let bad = b"{pas du json";
    let line = status_of(&c, &post(&heartbeat, Some("tok-a"), bad.len()), bad).await;
    assert_eq!(line.as_deref(), Some("HTTP/1.1 400 Bad Request"));
    c.shutdown().await;
}

#[tokio::test]
async fn ctl_32_metrics_are_served_on_the_api_port_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let c = start(config(dir.path())).await.unwrap();
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/metrics", c.api_addr()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("mimic_controller_leader 1"), "{text}");
    assert!(text.contains("mimic_controller_workloads"), "{text}");
    c.shutdown().await;
}
