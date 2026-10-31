//! One redirected connection (spec §4.1): original destination, connect, socket options and
//! protocol detection. Opaque traffic goes to [`crate::opaque`].

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::proxy::authority::normalise_authority;
use mimic_core::proxy::{DETECT_LIMIT, Detection, detect};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::access;
use crate::exchange::Ctx;
use crate::listener::{Direction, Outcome, Shared};
use crate::{http, opaque};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_IDLE: Duration = Duration::from_secs(300);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_PROBES: u32 = 4;
const READ_CHUNK: usize = 4096;
/// Default port of a TLS server name, removed by the authority normalisation.
const TLS_PORT: u16 = 443;

/// TCP keepalive (idle 300 s, interval 30 s, 4 probes) so that a vanished peer is noticed
/// without any idle timeout on live connections.
pub fn configure_socket(stream: &TcpStream) -> io::Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_PROBES);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// What the client and the destination said first.
enum First {
    /// The client's bytes so far decided the protocol.
    Client(Detection, Vec<u8>),
    /// The destination spoke first, or a side closed: an opaque tunnel with these bytes
    /// pending towards each side.
    Opaque {
        to_upstream: Vec<u8>,
        to_client: Vec<u8>,
    },
}

pub(crate) async fn handle(client: TcpStream, direction: Direction, shared: Arc<Shared>) {
    let started = Instant::now();
    let dst = match shared.original_dst(&client) {
        Ok(dst) if !shared.is_listener(dst) => dst,
        Ok(dst) => {
            shared.metrics().count(direction, Outcome::Loop);
            tracing::debug!(direction = direction.as_str(), original_dst = %dst, "loop, connection closed");
            return;
        }
        Err(e) => {
            shared.metrics().count(direction, Outcome::NoOriginalDst);
            tracing::debug!(direction = direction.as_str(), error = %e, "no original destination, connection closed");
            return;
        }
    };
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await;
    let upstream = match connected {
        Ok(Ok(upstream)) => upstream,
        failure => {
            shared.metrics().count(direction, Outcome::ConnectError);
            let error = match failure {
                Ok(Err(e)) => e.to_string(),
                _ => "connect timeout".to_string(),
            };
            tracing::debug!(direction = direction.as_str(), %dst, %error, "connect failed");
            return;
        }
    };
    shared.metrics().count(direction, Outcome::Ok);
    for socket in [&client, &upstream] {
        if let Err(e) = configure_socket(socket) {
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot set keepalive");
        }
    }
    let proxy = shared.proxy();
    let (mut client, mut upstream) = (client, upstream);
    let (protocol, sni, to_upstream, to_client) = match first_bytes(&mut client, &mut upstream)
        .await
    {
        First::Client(detection, buf) => {
            tracing::debug!(direction = direction.as_str(), %dst, ?detection, "protocol detected");
            match detection {
                Detection::Http1 => {
                    proxy.connection(direction, "http1", &dst.to_string());
                    let ctx = Ctx {
                        direction,
                        dst,
                        proxy: proxy.clone(),
                    };
                    if let Err(e) = http::relay(client, upstream, buf, ctx).await {
                        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "http relay ended");
                    }
                    return;
                }
                Detection::Tls { sni } => {
                    let sni = sni.map(|name| normalise_authority(&name, TLS_PORT));
                    ("tls", sni, buf, Vec::new())
                }
                // HTTP/2 is tunnelled like any other bytes until its relay exists.
                _ => ("tcp", None, buf, Vec::new()),
            }
        }
        First::Opaque {
            to_upstream,
            to_client,
        } => ("tcp", None, to_upstream, to_client),
    };
    let authority = sni.clone().unwrap_or_else(|| dst.to_string());
    proxy.connection(direction, protocol, &authority);
    let tunnelled = opaque::tunnel(&mut client, &mut upstream, to_upstream, to_client).await;
    if let Some(e) = &tunnelled.error {
        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "tunnel ended");
    }
    proxy.bytes(
        direction,
        protocol,
        tunnelled.to_upstream,
        tunnelled.to_client,
    );
    access::connection(&access::Connection {
        direction,
        protocol,
        dst,
        sni,
        outcome: tunnelled.outcome,
        duration: started.elapsed(),
        bytes_in: tunnelled.to_upstream,
        bytes_out: tunnelled.to_client,
    });
}

/// Reads until the protocol is known, with no timeout and within [`DETECT_LIMIT`]. The bytes
/// read are returned to be forwarded unchanged.
async fn first_bytes(client: &mut TcpStream, upstream: &mut TcpStream) -> First {
    let mut buf = Vec::new();
    loop {
        let mut from_client = [0u8; READ_CHUNK];
        let mut from_upstream = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            read = client.read(&mut from_client[..want]) => match read {
                Ok(n) if n > 0 => {
                    buf.extend_from_slice(&from_client[..n]);
                    match detect(&buf) {
                        Detection::NeedMore => {}
                        found => return First::Client(found, buf),
                    }
                }
                // Closed or reset: the tunnel propagates it after the pending bytes.
                _ => return First::Opaque { to_upstream: buf, to_client: Vec::new() },
            },
            read = upstream.read(&mut from_upstream) => {
                let n = read.unwrap_or(0);
                return First::Opaque {
                    to_upstream: buf,
                    to_client: from_upstream[..n].to_vec(),
                };
            }
        }
    }
}
