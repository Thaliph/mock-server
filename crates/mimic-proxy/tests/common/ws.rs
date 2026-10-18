//! Harnais WebSocket : serveur d'écho qui compte ses connexions, poignées de main écrites à
//! la main (spec §3.4).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mimic_proxy::Running;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

pub const FRAME: &str = "trame-confidentielle";

/// Serveur WebSocket d'écho : renvoie chaque message texte ou binaire tel quel.
pub struct EchoServer {
    pub addr: SocketAddr,
    /// Poignées de main WebSocket réussies.
    pub connections: Arc<AtomicUsize>,
    /// Connexions TCP acceptées, poignée de main réussie ou non.
    pub accepted: Arc<AtomicUsize>,
}

impl EchoServer {
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

pub async fn echo_server() -> EchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let (count, tcp) = (connections.clone(), accepted.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tcp.fetch_add(1, Ordering::SeqCst);
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
    EchoServer {
        addr,
        connections,
        accepted,
    }
}

/// Envoie une poignée de main écrite à la main, pour choisir la forme de la cible (chemin
/// seul en entrée, URI absolue vers le proxy sortant explicite). Renvoie le flux, l'en-tête
/// de la réponse et la clé envoyée, sans rien lire au-delà de l'en-tête.
pub async fn handshake(
    proxy: SocketAddr,
    target: &str,
    host: &str,
    extra: &[(&str, &str)],
) -> (TcpStream, String, String) {
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
    (stream, String::from_utf8(resp).unwrap(), key)
}

/// Poignée de main qui doit aboutir : `101` et `Sec-WebSocket-Accept` correct. Renvoie le
/// flux WebSocket et l'en-tête reçu.
pub async fn ws_connect(
    proxy: SocketAddr,
    target: &str,
    host: &str,
    extra: &[(&str, &str)],
) -> (WebSocketStream<TcpStream>, String) {
    let (stream, resp, key) = handshake(proxy, target, host, extra).await;
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

/// Poignée de main sortante qui reçoit une réponse HTTP ordinaire : en-tête et body (lu selon
/// `Content-Length`).
pub async fn outbound_refused(p: &Running, server: &EchoServer, path: &str) -> (String, String) {
    let host = server.addr.to_string();
    let target = format!("http://{host}{path}");
    let (mut stream, head, _) = handshake(p.outbound_addr(), &target, &host, &[]).await;
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut body))
        .await
        .expect("body de la réponse")
        .unwrap();
    (head, String::from_utf8(body).unwrap())
}

pub async fn next(ws: &mut WebSocketStream<TcpStream>) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("message attendu")
        .map(|m| m.unwrap())
}

/// Écho de messages texte et binaires, puis fermeture propre des deux côtés.
pub async fn echo_and_close(mut ws: WebSocketStream<TcpStream>, linger: Duration) {
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

pub async fn inbound_ws(
    p: &Running,
    path: &str,
    extra: &[(&str, &str)],
) -> WebSocketStream<TcpStream> {
    let host = p.inbound_addr().to_string();
    ws_connect(p.inbound_addr(), path, &host, extra).await.0
}

pub async fn outbound_ws(
    p: &Running,
    server: &EchoServer,
    path: &str,
    extra: &[(&str, &str)],
) -> (WebSocketStream<TcpStream>, String) {
    let host = server.addr.to_string();
    let target = format!("http://{host}{path}");
    ws_connect(p.outbound_addr(), &target, &host, extra).await
}

/// `mimic_websocket_connections_total` d'un sens et d'une issue (`relayed`, `refused`).
pub fn ws_metric(m: &str, direction: &str, result: &str) -> f64 {
    super::metric_value(
        m,
        &format!(
            r#"mimic_websocket_connections_total{{direction="{direction}",result="{result}"}}"#
        ),
    )
    .unwrap_or(0.0)
}
