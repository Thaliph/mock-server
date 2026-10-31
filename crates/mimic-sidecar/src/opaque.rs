//! The opaque tunnel (spec §4.1): bytes are relayed unchanged in both directions, with no idle
//! timeout. A FIN on one side reaches the other as a FIN; a reset (or any socket failure) on one
//! side reaches the other as a reset (`SO_LINGER` 0), also after a FIN. The bytes relayed and the
//! side that failed are recorded for the access line and the metrics.

use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use socket2::SockRef;
use tokio::io::Interest;
use tokio::net::TcpStream;

use crate::access::Outcome;

const CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Client,
    Upstream,
}

/// How a tunnel ended.
pub struct Tunnelled {
    /// Bytes read from the client, including those read by detection.
    pub to_upstream: u64,
    /// Bytes read from the destination, including those read by detection.
    pub to_client: u64,
    pub outcome: Outcome,
    pub error: Option<io::Error>,
}

pub(crate) async fn write_all(to: &TcpStream, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        to.writable().await?;
        match to.try_write(data) {
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copies `from` to `to` until `from` sends a FIN, which is passed on as a FIN. A failure is
/// returned with the side it happened on.
async fn copy(
    from: &TcpStream,
    from_side: Side,
    to: &TcpStream,
    to_side: Side,
    count: &AtomicU64,
) -> Result<(), (Side, io::Error)> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        from.readable().await.map_err(|e| (from_side, e))?;
        match from.try_read(&mut buf) {
            Ok(0) => {
                return SockRef::from(to)
                    .shutdown(Shutdown::Write)
                    .or_else(|e| match e.kind() {
                        io::ErrorKind::NotConnected => Ok(()),
                        _ => Err((to_side, e)),
                    });
            }
            Ok(n) => {
                count.fetch_add(n as u64, Ordering::Relaxed);
                write_all(to, &buf[..n]).await.map_err(|e| (to_side, e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err((from_side, e)),
        }
    }
}

/// Resolves when the socket has a pending error (a reset, also after a FIN was read).
pub(crate) async fn failed(s: &TcpStream, side: Side) -> (Side, io::Error) {
    loop {
        if let Err(e) = s.ready(Interest::ERROR).await {
            return (side, e);
        }
        match s.take_error() {
            Ok(Some(e)) => return (side, e),
            Err(e) => return (side, e),
            // Ready without an error: nothing to report, do not spin.
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Makes the close of `s` a reset.
pub(crate) fn abort(s: &TcpStream) {
    let _ = SockRef::from(s).set_linger(Some(Duration::ZERO));
}

/// What detection learned before the tunnel starts.
pub enum Early {
    /// A side failed (a reset) while the first bytes were read.
    Failed(Side, io::Error),
    /// The grace ended during detection.
    Shutdown,
}

/// Why a tunnel ended other than by both sides finishing.
enum Ended {
    Failed(Side, io::Error),
    Shutdown,
}

/// Writes the bytes read during detection to their side, then relays until both sides are done,
/// a side fails (the other one gets a reset) or `abort` resolves (both are closed).
pub async fn tunnel(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    to_upstream: Vec<u8>,
    to_client: Vec<u8>,
    early: Option<Early>,
    abort_signal: impl Future<Output = ()>,
) -> Tunnelled {
    let (client, upstream) = (&*client, &*upstream);
    let up_count = AtomicU64::new(to_upstream.len() as u64);
    let down_count = AtomicU64::new(to_client.len() as u64);
    let result = match early {
        Some(Early::Shutdown) => Err(Ended::Shutdown),
        early => {
            let (up, down) = tokio::join!(
                write_all(upstream, &to_upstream),
                write_all(client, &to_client)
            );
            match (up, down, early) {
                (Err(e), _, _) => Err(Ended::Failed(Side::Upstream, e)),
                (_, Err(e), _) => Err(Ended::Failed(Side::Client, e)),
                (_, _, Some(Early::Failed(side, e))) => Err(Ended::Failed(side, e)),
                _ => tokio::select! {
                    r = async {
                        tokio::try_join!(
                            copy(client, Side::Client, upstream, Side::Upstream, &up_count),
                            copy(upstream, Side::Upstream, client, Side::Client, &down_count),
                        )
                    } => r.map(|_| ()).map_err(|(side, e)| Ended::Failed(side, e)),
                    (side, e) = failed(client, Side::Client) => Err(Ended::Failed(side, e)),
                    (side, e) = failed(upstream, Side::Upstream) => Err(Ended::Failed(side, e)),
                    () = abort_signal => Err(Ended::Shutdown),
                },
            }
        }
    };
    let (outcome, error) = match result {
        Ok(()) => (Outcome::Ok, None),
        Err(Ended::Shutdown) => (Outcome::Shutdown, None),
        Err(Ended::Failed(side, e)) => {
            // The other side sees what this one did: a reset.
            match side {
                Side::Upstream => {
                    abort(client);
                    (Outcome::UpstreamReset, Some(e))
                }
                Side::Client => {
                    abort(upstream);
                    (Outcome::ClientAbort, Some(e))
                }
            }
        }
    };
    Tunnelled {
        to_upstream: up_count.load(Ordering::Relaxed),
        to_client: down_count.load(Ordering::Relaxed),
        outcome,
        error,
    }
}
