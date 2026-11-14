//! L4 balancing (spec §7.9): a connection that is not HTTP goes to a pod chosen per
//! connection, over a socket the byte relay then serves untouched.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mimic_core::mesh::retry::{Outcome, Planner};
use tokio::net::TcpStream;

use super::MeshState;
use super::pool::dial;
use super::router::{Backend, InFlight, Plan};
use crate::tls::intercept::Origin;
use crate::transport::Stream;

/// The connect timeout of the original destination: the relay's.
pub(crate) const CLUSTER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to a pod, or to the original destination after the pods failed.
pub(crate) struct Dialed<S = TcpStream> {
    pub stream: S,
    /// Where it goes: a pod or the original destination.
    pub upstream: SocketAddr,
    /// The connection counts as a request in flight to its pod for as long as this lives.
    pub hold: Option<InFlight>,
    /// The connects it took, this one included (spec §7.9 "Retries").
    pub attempts: u32,
}

/// A TLS connection to a pod, or to the original destination after the pods failed.
pub(crate) type DialedTls = Dialed<Stream>;

impl MeshState {
    /// Connects a connection to `backend` (spec §7.9 "Retries"): the pods the planner chooses,
    /// one not tried yet at each try, then `cluster`, the original destination (counted as a
    /// fallback), at most `1 + retries` tries. Nothing was sent on any of them, so the bytes the
    /// client sent can go to whichever answers.
    pub(crate) async fn dial(
        &self,
        backend: &Arc<Backend>,
        cluster: SocketAddr,
    ) -> io::Result<Dialed> {
        self.dial_with(backend, cluster, |stream, _| async move { Ok(stream) })
            .await
    }

    /// [`MeshState::dial`] for intercepted TLS (spec §7.9): the handshake with `origin` follows
    /// each connect, the certificate verified against its name; a failed verification is a
    /// connect failure of that pod, retried like one. A handshake that fails on the last try is
    /// an error (nothing was sent), never a connection in cleartext.
    pub(crate) async fn dial_tls(
        &self,
        backend: &Arc<Backend>,
        cluster: SocketAddr,
        origin: &Origin,
    ) -> io::Result<DialedTls> {
        self.dial_with(backend, cluster, |stream, timeout| async move {
            let tls = origin.connect(stream, timeout).await?;
            Ok(Stream::tls(tls.into()))
        })
        .await
    }

    /// The try loop of both: `finish` completes a connected socket (the TLS handshake).
    async fn dial_with<S, F, Fut>(
        &self,
        backend: &Arc<Backend>,
        cluster: SocketAddr,
        finish: F,
    ) -> io::Result<Dialed<S>>
    where
        F: Fn(TcpStream, Duration) -> Fut,
        Fut: Future<Output = io::Result<S>>,
    {
        let router = self.router();
        // A stale view, or one that says `"mesh": false`, has the original destination for its
        // only try.
        let direct = self.stale() || self.off();
        let mut planner = Planner::new(router.retries);
        let mut tries = 0;
        let mut retry = false;
        let mut last = None;
        loop {
            let plan = if direct {
                if tries == 0 {
                    Plan::Original
                } else {
                    Plan::Stop
                }
            } else {
                backend.plan(router, &mut planner)
            };
            let pick = match plan {
                Plan::Pod(pick) => Some(pick),
                Plan::Original => None,
                // A backend without an endpoint goes to the original destination.
                Plan::Stop if tries == 0 => None,
                Plan::Stop => {
                    return Err(last.unwrap_or_else(|| io::Error::other("no try left")));
                }
            };
            tries += 1;
            if retry {
                router.metrics.retry(&backend.label, Outcome::ConnectFailed);
            }
            let (upstream, timeout) = match &pick {
                Some(pick) => (pick.endpoint().addr, router.connect_timeout),
                None => {
                    router.metrics.fallback(&backend.label);
                    (cluster, CLUSTER_CONNECT_TIMEOUT)
                }
            };
            let connected = match dial(upstream, timeout).await {
                Ok(stream) => finish(stream, timeout).await,
                Err(e) => Err(e),
            };
            match connected {
                Ok(stream) => {
                    router.metrics.attempt(&backend.label, Outcome::Ok);
                    if let Some(pick) = &pick {
                        router.succeeded(pick);
                    }
                    return Ok(Dialed {
                        stream,
                        upstream,
                        hold: pick,
                        attempts: tries,
                    });
                }
                Err(e) => {
                    tracing::debug!(%upstream, error = %e, "connect to the upstream failed");
                    router
                        .metrics
                        .attempt(&backend.label, Outcome::ConnectFailed);
                    if let Some(pick) = &pick {
                        router.failed(pick);
                    }
                    planner.failed(Outcome::ConnectFailed);
                    retry = true;
                    last = Some(e);
                }
            }
        }
    }
}
