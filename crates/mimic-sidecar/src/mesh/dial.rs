//! L4 balancing (spec §7.9): a connection that is not HTTP goes to a pod chosen per
//! connection, over a socket the byte relay then serves untouched.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;

use super::MeshState;
use super::pool::dial;
use super::router::{Backend, InFlight};
use crate::tls::intercept::Origin;
use crate::transport::Stream;

/// The connect timeout of the original destination: the relay's.
pub(crate) const CLUSTER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to a pod, or to the original destination after the pods failed.
pub(crate) struct Dialed {
    pub stream: TcpStream,
    /// Where it goes: a pod or the original destination.
    pub upstream: SocketAddr,
    /// The connection counts as a request in flight to its pod for as long as this lives.
    pub hold: Option<InFlight>,
}

impl MeshState {
    /// Connects a connection to `backend`: the pod P2C chooses, one other pod when that connect
    /// fails, then `cluster`, the original destination (counted as a fallback). Nothing was
    /// sent on any of them, so the bytes the client sent can go to whichever answers.
    pub(crate) async fn dial(
        &self,
        backend: &Arc<Backend>,
        cluster: SocketAddr,
    ) -> io::Result<Dialed> {
        let router = self.router();
        if !self.stale() && !self.off() {
            let mut tried = None;
            for _ in 0..2 {
                let Some(pick) = (match tried {
                    None => backend.pick(router),
                    Some(failed) => backend.pick_other(router, failed),
                }) else {
                    break;
                };
                let addr = pick.endpoint().addr;
                match dial(addr, router.connect_timeout).await {
                    Ok(stream) => {
                        router.succeeded(&pick);
                        return Ok(Dialed {
                            stream,
                            upstream: addr,
                            hold: Some(pick),
                        });
                    }
                    Err(e) => {
                        tracing::debug!(upstream = %addr, error = %e, "connect to the pod failed");
                        router.failed(&pick);
                        tried = Some(addr);
                    }
                }
            }
        }
        router.metrics.fallback(&backend.label);
        let stream = dial(cluster, CLUSTER_CONNECT_TIMEOUT).await?;
        Ok(Dialed {
            stream,
            upstream: cluster,
            hold: None,
        })
    }
}

/// A TLS connection to a pod, or to the original destination after the pods failed.
pub(crate) struct DialedTls {
    pub stream: Stream,
    pub upstream: SocketAddr,
    pub hold: Option<InFlight>,
}

impl MeshState {
    /// [`MeshState::dial`] for intercepted TLS (spec §7.9): the handshake with `origin` follows
    /// the connect, the certificate verified against its name. A handshake that fails is an
    /// error (nothing was sent), never a connection in cleartext.
    pub(crate) async fn dial_tls(
        &self,
        backend: &Arc<Backend>,
        cluster: SocketAddr,
        origin: &Origin,
    ) -> io::Result<DialedTls> {
        let dialed = self.dial(backend, cluster).await?;
        let timeout = if dialed.hold.is_some() {
            self.router().connect_timeout
        } else {
            CLUSTER_CONNECT_TIMEOUT
        };
        let tls = origin.connect(dialed.stream, timeout).await?;
        Ok(DialedTls {
            stream: Stream::tls(tls.into()),
            upstream: dialed.upstream,
            hold: dialed.hold,
        })
    }
}
