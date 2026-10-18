//! Relais WebSocket de bout en bout : PRX-12 à PRX-14 (spec §3.4, §5.3 et §11.2).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use mimic_core::model::{Body, Exchange};
use mimic_proxy::Running;
use mimic_proxy::config::{Mode, Unmatched};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

const SECRET: &str = "s3cr3t-ws-token";
const FRAME: &str = "trame-confidentielle";

/// Serveur WebSocket d'écho : renvoie chaque message texte ou binaire tel quel.
struct EchoServer {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

async fn echo_server() -> EchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let count = count.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                // La réponse à une fermeture est envoyée par tungstenite.
                while let Some(Ok(msg)) = ws.next().await {
                    if (msg.is_text() || msg.is_binary()) && ws.send(msg).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    EchoServer { addr, connections }
}

/// Poignée de main écrite à la main, pour choisir la forme de la cible (chemin seul en entrée,
/// URI absolue vers le proxy sortant explicite). Renvoie le flux WebSocket et l'en-tête reçu.
async fn ws_connect(
    proxy: SocketAddr,
    target: &str,
    host: &str,
    extra: &[(&str, &str)],
) -> (WebSocketStream<TcpStream>, String) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    let key = tokio_tungstenite::tungstenite::handshake::client::generate_key();
    let mut head = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    // Lecture octet par octet : rien au-delà de l'en-tête ne doit être consommé.
    let mut resp = Vec::new();
    while !resp.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut b))
            .await
            .expect("réponse à la poignée de main")
            .unwrap();
        assert!(
            n > 0,
            "connexion fermée : {}",
            String::from_utf8_lossy(&resp)
        );
        resp.push(b[0]);
    }
    let resp = String::from_utf8(resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 101"), "{resp}");
    let accept = derive_accept_key(key.as_bytes());
    assert!(
        resp.to_ascii_lowercase().contains(&format!(
            "sec-websocket-accept: {}",
            accept.to_ascii_lowercase()
        )),
        "{resp}"
    );
    let ws = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
    (ws, resp)
}

async fn next(ws: &mut WebSocketStream<TcpStream>) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("message attendu")
        .map(|m| m.unwrap())
}

/// Écho de messages texte et binaires, puis fermeture propre des deux côtés.
async fn echo_and_close(mut ws: WebSocketStream<TcpStream>, linger: Duration) {
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    for msg in [
        Message::text("bonjour é"),
        Message::text(FRAME),
        Message::binary(vec![0u8, 1, 2, 255]),
        Message::binary(big),
    ] {
        ws.send(msg.clone()).await.unwrap();
        assert_eq!(next(&mut ws).await, Some(msg));
    }
    tokio::time::sleep(linger).await;
    ws.close(None).await.unwrap();
    assert!(matches!(next(&mut ws).await, Some(Message::Close(_))));
    assert_eq!(next(&mut ws).await, None, "flux terminé après la fermeture");
}

fn ws_config(server: &EchoServer, mode: Mode) -> mimic_proxy::config::Config {
    config(Some(server.addr), mode)
}

async fn inbound_ws(p: &Running, path: &str, extra: &[(&str, &str)]) -> WebSocketStream<TcpStream> {
    let host = p.inbound_addr().to_string();
    ws_connect(p.inbound_addr(), path, &host, extra).await.0
}

async fn outbound_ws(
    p: &Running,
    server: &EchoServer,
    path: &str,
    extra: &[(&str, &str)],
) -> (WebSocketStream<TcpStream>, String) {
    let host = server.addr.to_string();
    let target = format!("http://{host}{path}");
    ws_connect(p.outbound_addr(), &target, &host, extra).await
}

fn ws_metric(m: &str, direction: &str) -> f64 {
    metric_value(
        m,
        &format!(r#"mimic_websocket_connections_total{{direction="{direction}"}}"#),
    )
    .unwrap_or(0.0)
}

fn decode(v: &Value) -> Exchange {
    serde_json::from_value(v.clone()).unwrap()
}

#[tokio::test]
async fn prx_12_websocket_is_relayed_in_every_mode() {
    for mode in [
        Mode::Passthrough,
        Mode::Record,
        Mode::Mock,
        Mode::MockPartial,
    ] {
        let server = echo_server().await;
        let p = proxy(ws_config(&server, mode)).await;

        let ws = inbound_ws(&p, "/ws", &[]).await;
        echo_and_close(ws, Duration::ZERO).await;
        let (ws, _) = outbound_ws(&p, &server, "/ws", &[]).await;
        echo_and_close(ws, Duration::ZERO).await;

        assert_eq!(server.connections.load(Ordering::SeqCst), 2, "{mode:?}");
        let m = metrics(&p).await;
        assert_eq!(ws_metric(&m, "inbound"), 1.0, "{mode:?}\n{m}");
        assert_eq!(ws_metric(&m, "outbound"), 1.0, "{mode:?}\n{m}");
        assert!(
            m.contains(r#"method="GET",status="101"}"#),
            "{mode:?} : 101 compté dans mimic_requests_total\n{m}"
        );
    }
}

#[tokio::test]
async fn prx_13_record_keeps_the_handshake_not_the_frames() {
    let server = echo_server().await;
    let p = proxy(ws_config(&server, Mode::Record)).await;
    let auth = format!("Bearer {SECRET}");
    let extra = [("Authorization", auth.as_str())];
    let linger = Duration::from_millis(50);

    let ws = inbound_ws(&p, "/ws?room=7", &extra).await;
    echo_and_close(ws, linger).await;
    let (ws, _) = outbound_ws(&p, &server, "/ws?room=7", &extra).await;
    echo_and_close(ws, linger).await;

    for direction in ["inbound", "outbound"] {
        let list = wait_exchanges(&p, &format!("direction={direction}"), 1).await;
        assert_eq!(list.len(), 1, "{direction} : un échange par connexion");
        let e = decode(&list[0]);
        assert_eq!(e.method, "GET");
        assert_eq!(e.path, "/ws");
        assert_eq!(e.query, vec![("room".to_string(), "7".to_string())]);
        assert_eq!(e.status, 101);
        assert_eq!(e.req_body, Body::Empty, "{direction} : aucune trame");
        assert_eq!(e.resp_body, Body::Empty, "{direction} : aucune trame");
        let authz = e
            .req_header("authorization")
            .expect("header gardé, redacté");
        assert!(authz.starts_with("<redacted:"), "{authz}");
        assert!(
            e.req_header("upgrade")
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        );
        assert!(e.resp_header("sec-websocket-accept").is_some());
        assert!(e.total_us >= linger.as_micros() as u64, "{}", e.total_us);
        assert!(e.ttfb_us <= e.total_us);
    }
    let raw = client()
        .get(admin(&p, "/exchanges?limit=100"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!raw.contains(SECRET), "secret dans le store");
    assert!(!raw.contains(FRAME), "trame dans le store");

    // En passthrough, rien n'est enregistré.
    let server = echo_server().await;
    let p = proxy(ws_config(&server, Mode::Passthrough)).await;
    echo_and_close(inbound_ws(&p, "/ws", &[]).await, Duration::ZERO).await;
    echo_and_close(outbound_ws(&p, &server, "/ws", &[]).await.0, Duration::ZERO).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(exchanges(&p, "limit=100").await.is_empty());
}

#[tokio::test]
async fn prx_14_mock_relays_websocket_without_replaying_the_handshake() {
    for mode in [Mode::Mock, Mode::MockPartial] {
        let server = echo_server().await;
        let mut c = ws_config(&server, mode);
        c.mock.unmatched = Unmatched::Error;
        let p = proxy(c).await;

        let (ws, head) = outbound_ws(&p, &server, "/ws", &[]).await;
        assert!(
            !head.to_ascii_lowercase().contains("x-mimic-unmatched"),
            "{head}"
        );
        echo_and_close(ws, Duration::ZERO).await;
        assert_eq!(server.connections.load(Ordering::SeqCst), 1, "{mode:?}");
        let m = metrics(&p).await;
        assert!(
            !m.contains("mimic_mock_responses_total{"),
            "{mode:?} : la poignée de main ne passe pas par le moteur\n{m}"
        );

        // La poignée de main relayée est enregistrée, mais jamais rejouée.
        let list = wait_exchanges(&p, "direction=outbound", 1).await;
        assert_eq!(decode(&list[0]).status, 101);
        if mode == Mode::Mock {
            let r = app_client(&p)
                .get(format!("http://{}/ws", server.addr))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 599);
            assert_eq!(r.headers()["x-mimic-unmatched"], "true");
        }
    }
}
