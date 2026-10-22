//! Mock étanche (spec §3.1, §3.4) : en `mock`, le TCP sortant non HTTP et les connexions
//! restées muettes sont refusés par défaut, sans joindre la dépendance (PRX-23, PRX-24).
//!
//! Les connexions sont remises au sidecar comme le ferait l'écoute après une redirection
//! iptables (destination d'origine connue) ; `transparent.rs` refait le parcours avec de
//! vraies règles, en namespace réseau.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use mimic_proxy::Running;
use mimic_proxy::config::{Config, Mode, MockTcp};
use mimic_proxy::net::Protocol;
use mimic_proxy::proxy::{Silent, resolve_silent_within, tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Dépendance TCP qui compte les connexions acceptées et renvoie ce qu'elle reçoit, après
/// un salut éventuel (serveur qui parle le premier).
struct Dependency {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
}

impl Dependency {
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

async fn dependency(greeting: Option<&'static [u8]>) -> Dependency {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Some(g) = greeting {
                    let _ = s.write_all(g).await;
                }
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Dependency { addr, accepted }
}

/// Paire de connexions : côté application et côté sidecar, comme après l'`accept` de
/// l'écoute sortante.
async fn redirected() -> (TcpStream, TcpStream) {
    let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = TcpStream::connect(local.local_addr().unwrap())
        .await
        .unwrap();
    let (sidecar, _) = local.accept().await.unwrap();
    (app, sidecar)
}

/// Lit jusqu'à la fermeture par le sidecar ; `None` si la connexion reste ouverte.
async fn closed(stream: &mut TcpStream, wait: Duration) -> Option<Vec<u8>> {
    let mut data = vec![];
    let mut buf = [0u8; 1024];
    let deadline = Instant::now() + wait;
    loop {
        match tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            stream.read(&mut buf),
        )
        .await
        {
            Err(_) => return None,
            Ok(Ok(0)) | Ok(Err(_)) => return Some(data),
            Ok(Ok(n)) => data.extend_from_slice(&buf[..n]),
        }
    }
}

/// Connexion TCP sortante où l'application parle la première : renvoie ce que l'application
/// reçoit en retour (vide si le sidecar a refermé la connexion).
async fn client_first(p: &Running, dst: SocketAddr, inbound: bool, protocol: Protocol) -> Vec<u8> {
    let (mut app, sidecar) = redirected().await;
    let payload: &[u8] = match protocol {
        Protocol::Tls => &[0x16, 0x03, 0x01, 0x00],
        _ => b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary",
    };
    app.write_all(payload).await.unwrap();
    let state = p.state().clone();
    let relay = tokio::spawn(tunnel(state, sidecar, dst, protocol, inbound, None));
    let mut back = vec![0u8; payload.len()];
    let got = match tokio::time::timeout(Duration::from_secs(5), app.read_exact(&mut back)).await
    {
        Ok(Ok(_)) => back,
        Ok(Err(_)) => vec![],
        Err(_) => panic!("ni écho ni fermeture"),
    };
    drop(app);
    let _ = tokio::time::timeout(Duration::from_secs(5), relay).await;
    got
}

fn passthrough(m: &str, direction: &str, kind: &str, result: &str) -> f64 {
    metric_value(
        m,
        &format!(
            "mimic_passthrough_connections_total{{direction=\"{direction}\",kind=\"{kind}\",result=\"{result}\"}}"
        ),
    )
    .unwrap_or(0.0)
}

/// Journal de test : les lignes de log émises sur ce fil (runtime courant).
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
        let logs = Logs::default();
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    fn lines(&self, needle: &str) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|l| l.contains(needle))
            .map(String::from)
            .collect()
    }
}

fn mock_config(tcp: Option<MockTcp>) -> Config {
    let mut c = config(None, Mode::Mock);
    if let Some(tcp) = tcp {
        c.mock.tcp = tcp;
    }
    c
}

#[tokio::test]
async fn prx_23_non_http_tcp_is_refused_in_mock_by_default() {
    let (logs, _guard) = Logs::capture();
    let dep = dependency(None).await;
    let echo = b"\x00\x00\x00\x08\x04\xd2\x16\x2fbinary".to_vec();

    // record : relayée, octets identiques.
    let p = proxy(config(None, Mode::Record)).await;
    assert_eq!(client_first(&p, dep.addr, false, Protocol::Other).await, echo);
    assert_eq!(dep.accepted(), 1);

    // mock, `mock.tcp` absent : refusée sans joindre la dépendance.
    set_mode(&p, "mock").await;
    assert_eq!(Config::default().mock.tcp, MockTcp::Refuse);
    let got = client_first(&p, dep.addr, false, Protocol::Other).await;
    assert!(got.is_empty(), "connexion refermée en mock : {got:?}");
    assert_eq!(dep.accepted(), 1, "la dépendance n'est pas jointe");
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 1.0, "{m}");
    assert_eq!(passthrough(&m, "outbound", "tcp", "relayed"), 1.0, "{m}");
    let warned = logs.lines("refusée en mode mock");
    assert_eq!(warned.len(), 1, "{warned:#?}");
    assert!(warned[0].contains("WARN"), "{}", warned[0]);
    assert!(warned[0].contains(&dep.addr.to_string()), "{}", warned[0]);
    assert!(warned[0].contains("mock.tcp"), "{}", warned[0]);

    // TLS non intercepté : refusé de même, et journalisé.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let got = client_first(&p, dep.addr, false, Protocol::Tls).await;
    assert!(got.is_empty());
    assert_eq!(dep.accepted(), 1);
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tls", "refused"), 1.0, "{m}");
    let warned = logs.lines("refusée en mode mock");
    assert_eq!(warned.len(), 2, "{warned:#?}");
    assert!(warned[1].contains("tls") && warned[1].contains(&dep.addr.to_string()));

    // L'entrant n'est jamais concerné : relayé en mock (la sonde de l'adresse de
    // l'application compte aussi une connexion, §3.1).
    assert_eq!(client_first(&p, dep.addr, true, Protocol::Other).await, echo);
    assert!(dep.accepted() > 1);

    // mock-partial : relayée.
    set_mode(&p, "mock-partial").await;
    let before = dep.accepted();
    assert_eq!(client_first(&p, dep.addr, false, Protocol::Other).await, echo);
    assert_eq!(dep.accepted(), before + 1);

    // Rafale en mock : tous les refus comptés, au plus une ligne de log par seconde.
    set_mode(&p, "mock").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let started = Instant::now();
    for _ in 0..50 {
        assert!(
            client_first(&p, dep.addr, false, Protocol::Other)
                .await
                .is_empty()
        );
    }
    let elapsed = started.elapsed();
    assert_eq!(dep.accepted(), before + 1);
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 51.0, "{m}");
    let burst = logs.lines("refusée en mode mock").len() - 2;
    assert!(
        burst >= 1 && burst as u64 <= 1 + elapsed.as_secs(),
        "{burst} lignes en {elapsed:?}"
    );
    // La ligne suivante annonce les lignes omises.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        client_first(&p, dep.addr, false, Protocol::Other)
            .await
            .is_empty()
    );
    let last = logs.lines("refusée en mode mock").pop().unwrap();
    assert!(!last.contains("omitted=0"), "{last}");
    p.shutdown().await;

    // `mock.tcp: relay` : relayée en mock, mais le TLS non intercepté reste refusé.
    let p = proxy(mock_config(Some(MockTcp::Relay))).await;
    assert_eq!(client_first(&p, dep.addr, false, Protocol::Other).await, echo);
    assert_eq!(dep.accepted(), before + 2);
    assert!(
        client_first(&p, dep.addr, false, Protocol::Tls)
            .await
            .is_empty()
    );
    assert_eq!(dep.accepted(), before + 2);
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "relayed"), 1.0, "{m}");
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 0.0, "{m}");
    assert_eq!(passthrough(&m, "outbound", "tls", "refused"), 1.0, "{m}");
    p.shutdown().await;
}

/// Connexion muette : l'application ne dit rien (ou envoie `then` aussitôt), le sidecar
/// attend `wait`. Renvoie vrai si la connexion est rendue pour être traitée en HTTP, le
/// délai écoulé et ce que l'application a reçu avant la fermeture ou le salut.
async fn silent(
    p: &Running,
    dst: SocketAddr,
    wait: Duration,
    then: Option<&'static [u8]>,
) -> (bool, Duration, Vec<u8>) {
    let (mut app, sidecar) = redirected().await;
    let state = p.state().clone();
    let started = Instant::now();
    let decide = tokio::spawn(async move {
        match resolve_silent_within(state.clone(), sidecar, dst, false, wait).await {
            Silent::Client(_, Protocol::Http, _) => true,
            Silent::Client(s, protocol, upstream) => {
                tunnel(state, s, dst, protocol, false, upstream).await;
                false
            }
            Silent::Done => false,
        }
    });
    if let Some(bytes) = then {
        app.write_all(bytes).await.unwrap();
        let http = tokio::time::timeout(Duration::from_secs(10), decide)
            .await
            .unwrap()
            .unwrap();
        return (http, started.elapsed(), vec![]);
    }
    let mut buf = [0u8; 64];
    let got = match tokio::time::timeout(Duration::from_secs(10), app.read(&mut buf)).await {
        Ok(Ok(n)) => buf[..n].to_vec(),
        Ok(Err(_)) => vec![],
        Err(_) => panic!("ni salut ni fermeture"),
    };
    (false, started.elapsed(), got)
}

#[tokio::test]
async fn prx_24_silent_connection_is_refused_in_mock_after_the_wait() {
    let (logs, _guard) = Logs::capture();
    let dep = dependency(Some(b"220 ready\r\n")).await;
    let wait = Duration::from_millis(400);

    // mock, `mock.tcp` absent : refermée à la fin de l'attente, jamais avant.
    let p = proxy(mock_config(None)).await;
    let (http, at, got) = silent(&p, dep.addr, wait, None).await;
    assert!(!http);
    assert!(got.is_empty(), "connexion refermée : {got:?}");
    assert!(at >= wait, "refermée avant la fin de l'attente : {at:?}");
    assert_eq!(dep.accepted(), 0, "la dépendance n'est pas jointe");
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 1.0, "{m}");
    let warned = logs.lines("refusée en mode mock");
    assert_eq!(warned.len(), 1, "{warned:#?}");
    assert!(warned[0].contains("muette"), "{}", warned[0]);
    assert!(warned[0].contains(&dep.addr.to_string()), "{}", warned[0]);

    // En mock, du HTTP pendant l'attente : rendu pour être traité en HTTP, sans refus.
    let (http, _, _) = silent(
        &p,
        dep.addr,
        wait,
        Some(b"GET /users/1 HTTP/1.1\r\nhost: dep\r\n\r\n"),
    )
    .await;
    assert!(http, "connexion traitée en HTTP (PRX-15)");
    assert_eq!(dep.accepted(), 0);
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 1.0, "{m}");

    // mock-partial : la dépendance est jointe à la fin de l'attente seulement.
    set_mode(&p, "mock-partial").await;
    let (_, at, got) = silent(&p, dep.addr, wait, None).await;
    assert_eq!(got, b"220 ready\r\n");
    assert!(at >= wait, "{at:?}");
    assert_eq!(dep.accepted(), 1);
    p.shutdown().await;

    // `mock.tcp: relay` : relayée de même.
    let p = proxy(mock_config(Some(MockTcp::Relay))).await;
    let (_, at, got) = silent(&p, dep.addr, wait, None).await;
    assert_eq!(got, b"220 ready\r\n");
    assert!(at >= wait, "{at:?}");
    assert_eq!(dep.accepted(), 2);
    let m = metrics(&p).await;
    assert_eq!(passthrough(&m, "outbound", "tcp", "relayed"), 1.0, "{m}");
    assert_eq!(passthrough(&m, "outbound", "tcp", "refused"), 0.0, "{m}");
    p.shutdown().await;
}
