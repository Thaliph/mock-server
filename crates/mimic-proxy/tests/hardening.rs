//! Sidecar durci pour la production (spec §3.5, §5.2, §9.1, §9.2, §9.3) : délais vers
//! l'amont et côté serveur, plafond de connexions, accès à l'admin, budget des bodies en
//! vol, cardinalité de `method`, validation des périodes et sel de redaction.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use common::*;
use mimic_proxy::config::{Config, Mode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Destination qui ne répond pas au SYN : sa file d'acceptation est pleine, si bien que le
/// noyau jette les nouvelles demandes de connexion, comme une NetworkPolicy qui jette les
/// paquets. Les connexions rendues gardent la file pleine.
async fn blackhole() -> (SocketAddr, TcpListener, Vec<TcpStream>) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(0).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut held = vec![];
    for _ in 0..16 {
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => held.push(s),
            Ok(Err(e)) => panic!("connexion refusée au lieu d'être ignorée : {e}"),
            Err(_) => return (addr, listener, held),
        }
    }
    panic!("la file d'acceptation ne se remplit pas");
}

/// Application dont les réponses durent : `/drip` envoie dix fragments espacés de 100 ms,
/// `/hold/{n}` envoie 600 KiB d'un coup puis garde la réponse ouverte 600 ms.
async fn slow_app() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/drip",
            get(|| async {
                let stream = futures_util::stream::unfold(0u32, |i| async move {
                    if i == 10 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Some((
                        Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'd'; 1000])),
                        i + 1,
                    ))
                });
                Body::from_stream(stream)
            }),
        )
        .route(
            "/hold/{n}",
            get(|| async {
                let stream = futures_util::stream::unfold(0u32, |i| async move {
                    match i {
                        0 => Some((
                            Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'h'; 600 * 1024])),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(600)).await;
                            Some((Ok(bytes::Bytes::new()), 2))
                        }
                        _ => None,
                    }
                });
                Body::from_stream(stream)
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, handle)
}

/// Somme des séries d'une métrique dont la ligne contient `filter`.
fn sum(text: &str, name: &str, filter: &str) -> f64 {
    text.lines()
        .filter(|l| l.starts_with(name) && l.contains(filter))
        .filter_map(|l| l.rsplit_once(' ')?.1.parse::<f64>().ok())
        .sum()
}

/// Attend la fermeture de la connexion par le sidecar ; `None` si elle reste ouverte.
async fn closed_within(stream: &mut TcpStream, wait: Duration) -> Option<Duration> {
    let started = Instant::now();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(
            wait.saturating_sub(started.elapsed()),
            stream.read(&mut buf),
        )
        .await
        {
            Err(_) => return None,
            Ok(Ok(0)) | Ok(Err(_)) => return Some(started.elapsed()),
            Ok(Ok(_)) => {}
        }
    }
}

/// Envoie une requête HTTP/1.1 sur la connexion et lit la réponse (avec `content-length`).
async fn roundtrip(stream: &mut TcpStream, path: &str) -> String {
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: app\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut data = vec![];
    let mut buf = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("réponse attendue")
            .unwrap();
        assert!(n > 0, "connexion fermée avant la réponse");
        data.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&data).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= len {
                return text;
            }
        }
    }
}

#[tokio::test]
async fn prx_19_connect_timeout_bounds_outbound_connections() {
    assert_eq!(
        Config::default().outbound.connect_timeout,
        Duration::from_secs(10)
    );
    let (dst, _listener, _held) = blackhole().await;
    let mut c = config(None, Mode::Record);
    c.outbound.connect_timeout = Duration::from_millis(300);
    let p = proxy(c).await;

    // Requête HTTP : 504 dans le délai, et non au délai TCP du système.
    let started = Instant::now();
    let r = app_client(&p)
        .get(format!("http://{dst}/users/1"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r.status(), 504);
    assert!(
        r.text().await.unwrap().contains("outbound.connect_timeout"),
        "le body nomme l'option"
    );
    assert!(
        elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    let m = metrics(&p).await;
    assert_eq!(
        sum(
            &m,
            "mimic_upstream_errors_total{",
            "kind=\"connect_timeout\""
        ),
        1.0,
        "{m}"
    );

    // Tunnel CONNECT : refermé dans le délai, compté de même.
    let mut s = TcpStream::connect(p.outbound_addr()).await.unwrap();
    s.write_all(format!("CONNECT {dst} HTTP/1.1\r\nhost: {dst}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let closed = closed_within(&mut s, Duration::from_secs(3)).await;
    assert!(closed.is_some(), "tunnel CONNECT jamais refermé");
    let m = metrics(&p).await;
    assert_eq!(
        sum(
            &m,
            "mimic_upstream_errors_total{",
            "kind=\"connect_timeout\""
        ),
        2.0,
        "{m}"
    );

    // Tunnel d'une connexion redirigée : refermé, compté en erreur.
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut app_side = TcpStream::connect(local.local_addr().unwrap())
        .await
        .unwrap();
    let (sidecar_side, _) = local.accept().await.unwrap();
    let state = p.state().clone();
    let tunnel = tokio::time::timeout(
        Duration::from_secs(3),
        mimic_proxy::proxy::tunnel(
            state,
            sidecar_side,
            dst,
            mimic_proxy::net::Protocol::Other,
            false,
            None,
        ),
    )
    .await;
    assert!(tunnel.is_ok(), "tunnel transparent jamais abandonné");
    assert!(
        closed_within(&mut app_side, Duration::from_secs(1))
            .await
            .is_some()
    );
    let m = metrics(&p).await;
    assert_eq!(
        sum(
            &m,
            "mimic_passthrough_connections_total{",
            "result=\"error\""
        ),
        1.0,
        "{m}"
    );

    // Une connexion établie n'a pas de délai : la réponse lente est relayée en entier.
    let (slow, _h) = slow_app().await;
    let r = app_client(&p)
        .get(format!("http://{slow}/drip"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 10_000);
}

#[tokio::test]
async fn prx_20_header_read_timeout_closes_slow_and_idle_connections() {
    assert_eq!(
        Config::default().server.header_read_timeout,
        Duration::from_secs(60)
    );
    let (app, _h) = slow_app().await;
    let mut c = config(Some(app), Mode::Record);
    c.server.header_read_timeout = Duration::from_millis(300);
    let p = proxy(c).await;

    // En-têtes incomplets : connexion fermée après le délai.
    let mut s = TcpStream::connect(p.inbound_addr()).await.unwrap();
    s.write_all(b"GET /drip HTTP/1.1\r\nhost: app\r\n")
        .await
        .unwrap();
    let closed = closed_within(&mut s, Duration::from_secs(3)).await;
    assert!(
        closed.is_some_and(|d| d >= Duration::from_millis(250)),
        "{closed:?}"
    );

    // Connexion keep-alive inactive après une réponse : refermée de même.
    let fake = fake_server().await;
    let mut c = config(Some(fake.addr), Mode::Record);
    c.server.header_read_timeout = Duration::from_millis(300);
    let q = proxy(c).await;
    let mut s = TcpStream::connect(q.inbound_addr()).await.unwrap();
    assert!(
        roundtrip(&mut s, "/users/1")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        closed_within(&mut s, Duration::from_secs(3))
            .await
            .is_some(),
        "connexion inactive jamais refermée"
    );

    // Réponse streamée plus longue que le délai : relayée en entier.
    let r = client().get(inbound(&p, "/drip")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 10_000);
}

#[tokio::test]
async fn prx_21_connections_beyond_the_cap_are_refused() {
    assert_eq!(Config::default().server.max_connections, 2048);
    let fake = fake_server().await;
    let mut c = config(Some(fake.addr), Mode::Record);
    c.server.max_connections = 2;
    let p = proxy(c).await;
    let mut first = TcpStream::connect(p.inbound_addr()).await.unwrap();
    let mut second = TcpStream::connect(p.inbound_addr()).await.unwrap();
    assert!(
        roundtrip(&mut first, "/users/1")
            .await
            .starts_with("HTTP/1.1 200")
    );
    assert!(
        roundtrip(&mut second, "/users/2")
            .await
            .starts_with("HTTP/1.1 200")
    );

    for addr in [p.inbound_addr(), p.outbound_addr()] {
        let mut third = TcpStream::connect(addr).await.unwrap();
        let _ = third
            .write_all(b"GET /users/3 HTTP/1.1\r\nhost: app\r\n\r\n")
            .await;
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), third.read(&mut buf))
            .await
            .expect("connexion au-delà du plafond ni servie ni refermée")
            .unwrap_or(0);
        assert_eq!(n, 0, "aucune réponse au-delà du plafond");
    }
    let r = client().get(admin(&p, "/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200, "les sondes ont leur propre plafond");
    let m = metrics(&p).await;
    for listener in ["inbound", "outbound"] {
        assert_eq!(
            metric_value(
                &m,
                &format!("mimic_connections_refused_total{{listener=\"{listener}\"}}")
            ),
            Some(1.0),
            "{m}"
        );
    }

    // Une connexion fermée rend sa place.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut again = TcpStream::connect(p.inbound_addr()).await.unwrap();
    assert!(
        roundtrip(&mut again, "/users/4")
            .await
            .starts_with("HTTP/1.1 200")
    );
}

#[test]
fn prx_22_zero_periods_and_caps_are_rejected() {
    let load = |yaml: &str| {
        let yaml = yaml.to_string();
        Config::load_with(None, move |k| match k {
            "MIMIC_CONFIG_YAML" => Some(yaml.clone()),
            "MIMIC_WORKLOAD" => Some("api".into()),
            _ => None,
        })
    };
    assert!(load("share:\n  url: file:///tmp/mimic\n").is_ok());
    for (yaml, field) in [
        (
            "share:\n  url: file:///tmp/mimic\n  interval: 0\n",
            "share.interval",
        ),
        (
            "share:\n  url: file:///tmp/mimic\n  mode_poll: 0\n",
            "share.mode_poll",
        ),
        (
            "share:\n  url: file:///tmp/mimic\n  max_snapshot_bytes: 0\n",
            "share.max_snapshot_bytes",
        ),
        (
            "outbound:\n  connect_timeout: 0\n",
            "outbound.connect_timeout",
        ),
        (
            "server:\n  header_read_timeout: 0\n",
            "server.header_read_timeout",
        ),
        ("server:\n  max_connections: 0\n", "server.max_connections"),
        (
            "capture:\n  max_inflight_bytes: 0\n",
            "capture.max_inflight_bytes",
        ),
    ] {
        let err = load(yaml).expect_err(field);
        assert!(err.contains(field), "{field} : {err}");
    }
}

#[tokio::test]
async fn sec_07_admin_data_routes_answer_only_the_loopback() {
    let fake = fake_server().await;
    let mut c = config(Some(fake.addr), Mode::Record);
    c.admin.listen = "0.0.0.0:0".parse().unwrap();
    let p = proxy(c).await;
    let port = p.admin_addr().port();
    let url = |path: &str| format!("http://127.0.0.1:{port}{path}");
    client().get(inbound(&p, "/users/1")).send().await.unwrap();
    wait_exchanges(&p, "direction=inbound", 1).await;
    // Istio en mode sidecar relaie les autres pods vers l'application depuis 127.0.0.6.
    let remote = reqwest::Client::builder()
        .no_proxy()
        .local_address(std::net::IpAddr::from([127, 0, 0, 6]))
        .build()
        .unwrap();
    let local = client();
    let routes = [
        ("GET", "/"),
        ("GET", "/api/status"),
        ("GET", "/api/operations"),
        ("GET", "/openapi/inbound"),
        ("GET", "/openapi/outbound"),
        ("GET", "/exchanges"),
        ("GET", "/mode"),
        ("PUT", "/mode"),
        ("DELETE", "/store"),
        ("POST", "/share/sync"),
        ("GET", "/inconnu"),
    ];
    for (method, path) in routes {
        let r = remote
            .request(method.parse().unwrap(), url(path))
            .body("mock")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{method} {path} depuis 127.0.0.6");
    }
    // Rien n'a changé : mode, store.
    let mode = local.get(url("/mode")).send().await.unwrap();
    assert_eq!(mode.text().await.unwrap(), "record");
    assert_eq!(exchanges(&p, "direction=inbound").await.len(), 1);
    // Depuis la boucle locale, les routes de lecture répondent.
    for path in [
        "/",
        "/api/status",
        "/api/operations",
        "/openapi/inbound",
        "/openapi/outbound",
        "/exchanges",
    ] {
        let r = local.get(url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "GET {path} depuis 127.0.0.1");
    }
    assert_eq!(
        local.get(url("/inconnu")).send().await.unwrap().status(),
        404
    );
    // Sondes et métriques : depuis toute adresse.
    for path in ["/healthz", "/readyz", "/metrics"] {
        let r = remote.get(url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200, "GET {path} depuis 127.0.0.6");
    }
}

#[tokio::test]
async fn adm_06_nonstandard_methods_are_counted_as_other() {
    let fake = fake_server().await;
    let p = proxy(config(Some(fake.addr), Mode::Record)).await;
    let mut methods = vec!["PROPFIND".to_string()];
    methods.extend((0..20).map(|i| format!("XMETH{i}")));
    for m in &methods {
        client()
            .request(m.parse().unwrap(), inbound(&p, "/users/1"))
            .send()
            .await
            .unwrap();
    }
    let m = metrics(&p).await;
    assert_eq!(
        sum(&m, "mimic_requests_total{", "method=\"other\""),
        21.0,
        "{m}"
    );
    assert!(
        !m.contains("PROPFIND") && !m.contains("XMETH"),
        "aucune série par méthode inventée : {m}"
    );
}

#[tokio::test]
async fn cap_11_inflight_copies_are_bounded_without_slowing_traffic() {
    assert_eq!(
        Config::default().capture.max_inflight_bytes,
        64 * 1024 * 1024
    );
    let (app, _h) = slow_app().await;
    let mut c = config(Some(app), Mode::Record);
    // Tient une réponse de 600 KiB, pas deux.
    c.capture.max_inflight_bytes = 1024 * 1024;
    c.capture.max_body_bytes = 2 * 1024 * 1024;
    let p = proxy(c).await;
    let get = |n: u32| {
        let url = inbound(&p, &format!("/hold/{n}"));
        async move {
            client()
                .get(url)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }
    };
    let started = Instant::now();
    let (a, b) = tokio::join!(get(1), get(2));
    assert_eq!((a.len(), b.len()), (600 * 1024, 600 * 1024));
    assert!(started.elapsed() < Duration::from_secs(3), "trafic ralenti");
    let list = wait_exchanges(&p, "direction=inbound", 2).await;
    let kinds: Vec<&str> = list
        .iter()
        .map(|e| e["resp_body"]["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"truncated"), "{kinds:?}");
    let m = metrics(&p).await;
    assert!(
        metric_value(
            &m,
            "mimic_capture_dropped_total{reason=\"inflight_budget\"}"
        )
        .is_some_and(|v| v >= 1.0),
        "{m}"
    );

    // Budget rendu : une requête seule est capturée en entier.
    assert_eq!(get(3).await.len(), 600 * 1024);
    let list = wait_exchanges(&p, "direction=inbound", 3).await;
    let third = list.iter().find(|e| e["path"] == "/hold/3").unwrap();
    assert_eq!(third["resp_body"]["kind"], "captured", "{third}");
}

/// Journal capturé par un abonné `tracing` de test.
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
async fn red_06_missing_salt_is_announced_at_startup() {
    for salt in [None, Some("sel-du-workload".to_string())] {
        let logs = Logs::default();
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut c = config(None, Mode::Record);
        c.redact.salt = salt.clone();
        let p = proxy(c).await;
        drop(guard);
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let warned = text
            .lines()
            .any(|l| l.contains("WARN") && l.contains("MIMIC_REDACT_SALT"));
        assert_eq!(warned, salt.is_none(), "sel {salt:?} : {text}");
        p.shutdown().await;
    }
}
