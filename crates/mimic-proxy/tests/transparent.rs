//! Interception transparente réelle (PRX-04, PRX-04b, PRX-08, PRX-15, PRX-23, INI-04, INI-12).
//!
//! Exige root dans un namespace réseau jetable : lancer `scripts/e2e-netns.sh`.
//! Sans `MIMIC_NETNS_E2E=1`, le test ne fait rien (il ne doit jamais toucher l'iptables de l'hôte).

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use common::*;
use serde_json::Value;

const ADMIN: &str = "http://127.0.0.1:15100";

struct Sidecar(Child);

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn bin(name: &str) -> PathBuf {
    let proxy = PathBuf::from(env!("CARGO_BIN_EXE_mimic-proxy"));
    proxy.with_file_name(name)
}

/// Pose les règles. Les dépendances factices écoutent sur la boucle locale : sans
/// `loopback`, rien n'est intercepté (spec §3.1, INI-12).
fn init_rules(loopback: bool) {
    let mut cmd = Command::new(bin("mimic-init"));
    cmd.args(["--inbound-ports", ""]);
    if loopback {
        cmd.arg("--intercept-loopback");
    }
    let status = cmd
        .status()
        .expect("mimic-init (cargo build --workspace d'abord)");
    assert!(status.success());
}

fn iptables_rules() -> String {
    String::from_utf8(
        Command::new("iptables")
            .args(["-t", "nat", "-S"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

/// Serveur TCP qui renvoie ce qu'il reçoit, en saluant d'abord si `greeting` est fourni.
fn tcp_server(greeting: Option<&'static [u8]>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            std::thread::spawn(move || {
                if let Some(g) = greeting {
                    let _ = s.write_all(g);
                }
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 || s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// Connexion ouverte d'avance, comme le fait un pool : muette au-delà du délai de détection,
/// puis une requête HTTP. Renvoie la réponse brute. Hors du runtime, qui sert la dépendance.
async fn silent_then_get(port: u16, path: &'static str) -> String {
    tokio::task::spawn_blocking(move || silent_then_get_blocking(port, path))
        .await
        .unwrap()
}

fn silent_then_get_blocking(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

async fn wait_admin() {
    let deadline = Instant::now() + Duration::from_secs(10);
    while client()
        .get(format!("{ADMIN}/healthz"))
        .send()
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "le sidecar ne répond pas");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// INI-04, INI-12, PRX-04, PRX-04b, PRX-08, PRX-15 et PRX-23 : un seul scénario, qui pose de
/// vraies règles iptables ; il ne tourne que dans le namespace jetable de
/// `scripts/e2e-netns.sh` (`--ignored`, job CI `e2e-netns`).
#[tokio::test]
#[ignore = "iptables réel : scripts/e2e-netns.sh"]
async fn ini_04_12_prx_04_04b_08_15_23_transparent_interception_in_network_namespace() {
    assert_eq!(
        std::env::var("MIMIC_NETNS_E2E").as_deref(),
        Ok("1"),
        "à lancer par scripts/e2e-netns.sh, dans un namespace réseau jetable"
    );
    // INI-04 : deux applications, aucune règle dupliquée.
    init_rules(true);
    let first = iptables_rules();
    init_rules(true);
    assert_eq!(first, iptables_rules());
    assert_eq!(first.matches("-j MIMIC_OUTBOUND").count(), 1, "{first}");

    let dir = tempfile::tempdir().unwrap();
    // Le sidecar tourne sous un autre UID : il doit pouvoir lire sa configuration.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let ca_dir = dir.path().join("ca");
    std::fs::create_dir(&ca_dir).unwrap();
    std::fs::set_permissions(&ca_dir, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    let (https, https_pem) = fake_https_server().await;
    let upstream_pem = dir.path().join("upstream.pem");
    std::fs::write(&upstream_pem, &https_pem).unwrap();
    let config = dir.path().join("mimic.yaml");
    std::fs::write(
        &config,
        format!(
            "mode: record\ninstance: e2e-0\ninbound:\n  enabled: false\noutbound:\n  listen: 127.0.0.1:15101\nadmin:\n  listen: 127.0.0.1:15100\ntls:\n  intercept: [localhost]\n  ca_dir: {}\n  upstream_ca_file: {}\n",
            ca_dir.display(),
            upstream_pem.display()
        ),
    )
    .unwrap();
    let _sidecar = Sidecar(
        Command::new("setpriv")
            .args(["--reuid=1338", "--regid=1338", "--clear-groups"])
            .arg(env!("CARGO_BIN_EXE_mimic-proxy"))
            .arg("--config")
            .arg(&config)
            .spawn()
            .unwrap(),
    );
    wait_admin().await;

    // PRX-08 : l'application appelle sa dépendance sans aucun proxy configuré.
    let saas = fake_server().await;
    let url = format!("http://127.0.0.1:{}/users/4", saas.addr.port());
    let r = client().get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let deadline = Instant::now() + Duration::from_secs(5);
    let recorded = loop {
        let list: Vec<Value> = client()
            .get(format!("{ADMIN}/exchanges?direction=outbound"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(e) = list.into_iter().next() {
            break e;
        }
        assert!(Instant::now() < deadline, "aucun échange enregistré");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        recorded["authority"],
        format!("127.0.0.1:{}", saas.addr.port())
    );
    assert_eq!(recorded["path"], "/users/4");

    // PRX-15 : connexion HTTP muette plus de 300 ms, traitée en HTTP et enregistrée.
    let r = silent_then_get(saas.addr.port(), "/users/7").await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let list: Vec<Value> = client()
            .get(format!("{ADMIN}/exchanges?direction=outbound"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if list.iter().any(|e| e["path"] == "/users/7") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "connexion muette non traitée en HTTP"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // PRX-06 en transparent : HTTPS intercepté d'après le SNI, sans proxy configuré.
    let ca = std::fs::read(ca_dir.join("ca.crt")).unwrap();
    let tls_app = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap();
    let https_url = format!("https://localhost:{}/users/5", https.addr.port());
    let r = tls_app.get(&https_url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let list: Vec<Value> = client()
            .get(format!("{ADMIN}/exchanges?direction=outbound"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if list
            .iter()
            .any(|e| e["scheme"] == "https" && e["path"] == "/users/5")
        {
            break;
        }
        assert!(Instant::now() < deadline, "échange HTTPS non enregistré");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // PRX-04 : protocole où le client parle en premier, octets identiques.
    let echo = tcp_server(None);
    let mut s = TcpStream::connect(("127.0.0.1", echo)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary")
        .unwrap();
    let mut buf = [0u8; 14];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary");

    // PRX-04 : protocole où le serveur parle en premier.
    let greeter = tcp_server(Some(b"220 ready\r\n"));
    let mut s = TcpStream::connect(("127.0.0.1", greeter)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut greet = [0u8; 11];
    s.read_exact(&mut greet).unwrap();
    assert_eq!(&greet, b"220 ready\r\n");

    // PRX-04b : TLS relayé en record…
    let tls = tcp_server(None);
    let mut s = TcpStream::connect(("127.0.0.1", tls)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&[0x16, 0x03, 0x01, 0x00]).unwrap();
    let mut back = [0u8; 4];
    s.read_exact(&mut back).unwrap();
    assert_eq!(back, [0x16, 0x03, 0x01, 0x00]);

    // … puis refusé en mock, et la dépendance HTTP servie sans la joindre.
    let r = client()
        .put(format!("{ADMIN}/mode"))
        .body("mock")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let mut s = TcpStream::connect(("127.0.0.1", tls)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&[0x16, 0x03, 0x01, 0x00]).unwrap();
    let mut buf = [0u8; 4];
    assert!(
        matches!(s.read(&mut buf), Ok(0) | Err(_)),
        "connexion TLS fermée en mock"
    );

    // PRX-23 : TCP non HTTP refusé en mock (`mock.tcp` absent), sans écho de la dépendance.
    let mut s = TcpStream::connect(("127.0.0.1", echo)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary")
        .unwrap();
    let mut buf = [0u8; 14];
    assert!(
        matches!(s.read(&mut buf), Ok(0) | Err(_)),
        "connexion TCP fermée en mock"
    );

    let hits = saas.hits();
    saas.stop();
    let r = client().get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], 4);
    assert_eq!(saas.hits(), hits);

    // PRX-15 en mock : connexion muette servie depuis le store, sans joindre la dépendance.
    let r = silent_then_get(saas.addr.port(), "/users/4").await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert_eq!(saas.hits(), hits);

    // HTTPS intercepté servi en mock, sans joindre la dépendance.
    let https_hits = https.hits();
    https.stop();
    let r = tls_app.get(&https_url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["id"], 5);
    assert_eq!(https.hits(), https_hits);

    let m = client()
        .get(format!("{ADMIN}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(m.contains(r#"mimic_passthrough_connections_total{direction="outbound",kind="tls",result="refused"}"#), "{m}");
    // PRX-04 : deux tunnels (client puis serveur qui parle en premier) ; PRX-15 : aucun de plus.
    assert!(
        m.contains(r#"mimic_passthrough_connections_total{direction="outbound",kind="tcp",result="relayed"} 2"#),
        "{m}"
    );
    let refused = r#"mimic_passthrough_connections_total{direction="outbound",kind="tcp",result="refused"} 1"#;
    assert!(m.contains(refused), "{m}");

    // INI-12 : règles par défaut, la boucle locale n'est plus interceptée. Le sidecar reste en
    // mock, et la même connexion TCP arrive à son serveur sans passer par lui.
    init_rules(false);
    let rules = iptables_rules();
    assert!(rules.contains("-A MIMIC_OUTBOUND -o lo -j RETURN"), "{rules}");
    assert!(!rules.contains("! -d 127.0.0.1/32"), "{rules}");
    let mut s = TcpStream::connect(("127.0.0.1", echo)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary")
        .unwrap();
    let mut buf = [0u8; 14];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary");
    let m = client()
        .get(format!("{ADMIN}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(m.contains(refused), "aucun refus de plus : {m}");
}
