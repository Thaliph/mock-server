//! One redirected connection (spec §4.1): original destination, connect, socket options and
//! protocol detection. Opaque traffic goes to [`crate::opaque`].

use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::proxy::authority::normalise_authority;
use mimic_core::proxy::{DETECT_LIMIT, Detection, detect};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::access;
use crate::exchange::{Ctx, Http1Tracker};
use crate::h2;
use crate::http1;
use crate::listener::{Active, Direction, Outcome, Shared};
use crate::opaque::{self, Early, Side};
use crate::transport::Stream;

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
        /// A reset, or the end of the grace, seen while reading.
        early: Option<Early>,
    },
}

pub(crate) async fn handle(
    client: TcpStream,
    direction: Direction,
    shared: Arc<Shared>,
    _active: Active,
) {
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
    let mut abort = pin!(shared.aborted());
    let ctx = |dst| Ctx {
        direction,
        dst,
        proxy: proxy.clone(),
        aborted: shared.abort_flag(),
    };
    let (protocol, sni, to_upstream, to_client, early) = match first_bytes(
        &mut client,
        &mut upstream,
        abort.as_mut(),
    )
    .await
    {
        First::Client(detection, buf) => {
            tracing::debug!(direction = direction.as_str(), %dst, ?detection, "protocol detected");
            match detection {
                Detection::Http1 => {
                    proxy.connection(direction, "http1", &dst.to_string());
                    // At the end of the grace the relay closes both sockets, and the exchanges
                    // still open write their `shutdown` line.
                    let mut tracker = Http1Tracker::new(ctx(dst));
                    let (mut client, mut upstream) =
                        (Stream::plain(client), Stream::plain(upstream));
                    let relayed = http1::relay(
                        &mut client,
                        &mut upstream,
                        buf,
                        |event| tracker.event(event),
                        shared.stopped(),
                        abort.as_mut(),
                    )
                    .await;
                    tracker.finish(relayed.outcome, relayed.partial_body);
                    if let Some(e) = &relayed.error {
                        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "http relay ended");
                    }
                    return;
                }
                Detection::Tls { sni } => {
                    let sni = sni.map(|name| normalise_authority(&name, TLS_PORT));
                    ("tls", sni, buf, Vec::new(), None)
                }
                Detection::H2 => {
                    proxy.connection(direction, "h2", &dst.to_string());
                    let relayed = tokio::select! {
                        r = h2::relay(Stream::plain(client), Stream::plain(upstream), buf, ctx(dst), shared.stopped()) => r,
                        () = abort.as_mut() => Ok(()),
                    };
                    if let Err(e) = relayed {
                        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "h2 relay ended");
                    }
                    return;
                }
                _ => ("tcp", None, buf, Vec::new(), None),
            }
        }
        First::Opaque {
            to_upstream,
            to_client,
            early,
        } => ("tcp", None, to_upstream, to_client, early),
    };
    let authority = sni.clone().unwrap_or_else(|| dst.to_string());
    proxy.connection(direction, protocol, &authority);
    let tunnelled = opaque::tunnel(
        &mut client,
        &mut upstream,
        to_upstream,
        to_client,
        early,
        abort.as_mut(),
    )
    .await;
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
/// read are returned to be forwarded unchanged. A reset on either side is returned as such, to
/// reach the other side as a reset; the end of the grace (`abort`) ends the detection.
async fn first_bytes(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    abort: impl Future<Output = ()>,
) -> First {
    let mut abort = pin!(abort);
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
                // Closed: the tunnel propagates the FIN after the pending bytes.
                Ok(_) => return First::Opaque { to_upstream: buf, to_client: Vec::new(), early: None },
                Err(e) => {
                    let early = Some(Early::Failed(Side::Client, e));
                    return First::Opaque { to_upstream: buf, to_client: Vec::new(), early };
                }
            },
            read = upstream.read(&mut from_upstream) => {
                let (n, early) = match read {
                    Ok(n) => (n, None),
                    Err(e) => (0, Some(Early::Failed(Side::Upstream, e))),
                };
                return First::Opaque {
                    to_upstream: buf,
                    to_client: from_upstream[..n].to_vec(),
                    early,
                };
            }
            () = &mut abort => {
                return First::Opaque {
                    to_upstream: buf,
                    to_client: Vec::new(),
                    early: Some(Early::Shutdown),
                };
            }
        }
    }
}
