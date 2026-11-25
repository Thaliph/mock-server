//! The listeners of the gateway role (spec §7.10): the bind, one accept loop per socket under a
//! single cap, the timers of each connection's phase, and the drain at `SIGTERM`. Every request
//! is answered `404` `x-mimic-error: no-route`, its body not read.

use std::convert::Infallible;
use std::fmt;
use std::future::{Future, pending};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_LENGTH, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use mimic_core::mesh::reason::Reason;
use mimic_core::proxy::H2_PREFACE;
use mimic_runtime::admin::Readiness;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

use super::config::GatewayConfig;
use crate::listener;
use crate::transport::Stream;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);
const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(1);
/// Time given to the connections to close once the grace is over: dropping them is immediate.
const ABORT_WAIT: Duration = Duration::from_millis(200);
/// Most bytes of a request head (and of hyper's read buffer).
const MAX_HEAD: usize = 64 * 1024;
const H2_STREAMS: u32 = 100;
const H2_HEADER_LIST: u32 = 64 * 1024;
const H2_PENDING_ACCEPT_RESETS: usize = 20;
const H2_LOCAL_ERROR_RESETS: usize = 1024;

/// A port that could not be bound.
#[derive(Debug)]
pub struct BindError {
    pub port: u16,
    pub addr: SocketAddr,
    pub source: io::Error,
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bind gateway listener {}: {}", self.addr, self.source)
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// State shared by the listeners and the connections of the role.
pub struct Gateway {
    config: GatewayConfig,
    /// Set once the last socket is bound.
    bound: AtomicBool,
    /// Set at [`Gateway::shutdown`].
    stopping: AtomicBool,
    /// One permit per connection served at once, over all listeners.
    slots: Arc<Semaphore>,
    active: watch::Sender<usize>,
    stop: watch::Sender<bool>,
    abort: watch::Sender<bool>,
}

/// Counts a connection as active until dropped, and holds its slot under the cap.
struct Active(
    watch::Sender<usize>,
    #[allow(dead_code)] OwnedSemaphorePermit,
);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Arc<Gateway> {
        Arc::new(Gateway {
            slots: Arc::new(Semaphore::new(config.max_connections)),
            config,
            bound: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            active: watch::channel(0).0,
            stop: watch::channel(false).0,
            abort: watch::channel(false).0,
        })
    }

    /// Binds `0.0.0.0:<port>`, then `[::]:<port>` when `ipv6`, for each port in order.
    pub fn bind(&self, ipv6: bool) -> Result<Vec<(TcpListener, SocketAddr)>, BindError> {
        let mut bound = Vec::new();
        for &port in &self.config.ports {
            let v4 = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
            let v6 = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);
            for addr in std::iter::once(v4).chain(ipv6.then_some(v6)) {
                let error = |source| BindError { port, addr, source };
                let listener = listener::bind(addr).map_err(error)?;
                let local = listener.local_addr().map_err(error)?;
                tracing::info!(addr = %local, "gateway listening");
                bound.push((listener, local));
            }
        }
        self.bound.store(true, Ordering::Release);
        Ok(bound)
    }

    /// `/readyz`: `Ok` once every socket is bound, until the shutdown.
    pub fn readiness(self: &Arc<Self>) -> Readiness {
        let gateway = self.clone();
        Readiness::new(move || {
            let ready = if gateway.stopping.load(Ordering::Acquire) {
                Err("shutting down".to_string())
            } else if !gateway.bound.load(Ordering::Acquire) {
                Err("listeners not bound".to_string())
            } else {
                Ok(())
            };
            async move { ready }
        })
    }

    /// Accepts connections on each listener until [`Gateway::shutdown`].
    pub fn spawn(self: &Arc<Self>, listeners: Vec<(TcpListener, SocketAddr)>) {
        for (listener, _) in listeners {
            tokio::spawn(accept_loop(self.clone(), listener));
        }
    }

    /// Starts the shutdown: readiness fails, the accept loops end and drop their listeners, the
    /// connections close once their requests are answered.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.stop.send_replace(true);
    }

    /// Waits at most `grace` for the connections to end, then drops the ones left.
    pub async fn drain(&self, grace: Duration) {
        let mut active = self.active.subscribe();
        if tokio::time::timeout(grace, active.wait_for(|n| *n == 0))
            .await
            .is_ok()
        {
            return;
        }
        let connections = *active.borrow();
        self.abort.send_replace(true);
        tracing::warn!(connections, "grace period over");
        let _ = tokio::time::timeout(ABORT_WAIT, active.wait_for(|n| *n == 0)).await;
    }

    fn track(&self, slot: OwnedSemaphorePermit) -> Active {
        self.active.send_modify(|n| *n += 1);
        Active(self.active.clone(), slot)
    }
}

/// Accepts on `listener` until the shutdown, then drops it: a new connect is refused. A
/// connection with no slot left is closed at once.
async fn accept_loop(gateway: Arc<Gateway>, listener: TcpListener) {
    let mut stop = gateway.stop.subscribe();
    let mut last_logged: Option<Instant> = None;
    loop {
        let accepted = tokio::select! {
            _ = stop.wait_for(|stop| *stop) => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                let Ok(slot) = gateway.slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let active = gateway.track(slot);
                tokio::spawn(connection(gateway.clone(), stream, active));
            }
            Err(e) => {
                if last_logged.is_none_or(|t| t.elapsed() >= ACCEPT_LOG_INTERVAL) {
                    last_logged = Some(Instant::now());
                    tracing::warn!(error = %e, "accept failed");
                }
                tokio::select! {
                    _ = stop.wait_for(|stop| *stop) => return,
                    () = tokio::time::sleep(ACCEPT_RETRY) => {}
                }
            }
        }
    }
}

/// Where a connection is, for its timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Waiting for a request head since the instant (the accept, or the first byte after an
    /// answer).
    Head(Instant),
    /// That many requests being answered.
    Busy(usize),
    /// Every request answered at the instant, no byte since.
    Idle(Instant),
}

/// The instant the connection is closed at, in `phase`.
pub(crate) fn deadline(phase: &Phase, header: Duration, idle: Duration) -> Option<Instant> {
    match *phase {
        Phase::Head(since) => Some(since + header),
        Phase::Idle(since) => Some(since + idle),
        Phase::Busy(_) => None,
    }
}

/// Serves one accepted connection, HTTP/2 when it starts with the preface, else HTTP/1.1.
async fn connection(gateway: Arc<Gateway>, mut tcp: TcpStream, _active: Active) {
    let _ = tcp.set_nodelay(true);
    let (header, idle) = (gateway.config.header_timeout, gateway.config.idle_timeout);
    let accepted = Instant::now();
    let mut stop = gateway.stop.subscribe();
    let abort = gateway.abort.subscribe();
    let mut first = Vec::with_capacity(H2_PREFACE.len());
    let h2 = tokio::select! {
        detected = tokio::time::timeout_at(accepted + header, detect(&mut tcp, &mut first)) => {
            match detected {
                Ok(Some(h2)) => h2,
                // EOF, an error, or the header deadline.
                _ => return,
            }
        }
        _ = stop.wait_for(|stop| *stop) => return,
    };
    let phase = watch::channel(Phase::Head(accepted)).0;
    let stream = Stream::plain_with_prefix(tcp, first);
    let service = {
        let phase = phase.clone();
        service_fn(move |_: Request<Incoming>| {
            phase.send_modify(|p| {
                *p = match *p {
                    Phase::Busy(n) => Phase::Busy(n + 1),
                    _ => Phase::Busy(1),
                }
            });
            let answer = no_route();
            phase.send_modify(|p| {
                *p = match *p {
                    Phase::Busy(n) if n > 1 => Phase::Busy(n - 1),
                    _ => Phase::Idle(Instant::now()),
                }
            });
            std::future::ready(Ok::<_, Infallible>(answer))
        })
    };
    let timers = Timers {
        phase: phase.clone(),
        header,
        idle,
    };
    if h2 {
        phase.send_replace(Phase::Idle(Instant::now()));
        let served = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .auto_date_header(false)
            .max_concurrent_streams(H2_STREAMS)
            .max_header_list_size(H2_HEADER_LIST)
            .max_pending_accept_reset_streams(H2_PENDING_ACCEPT_RESETS)
            .max_local_error_reset_streams(H2_LOCAL_ERROR_RESETS)
            .serve_connection(TokioIo::new(stream), service);
        drive(served, |c| c.graceful_shutdown(), timers, stop, abort).await;
    } else {
        let watched = Watched {
            inner: stream,
            phase,
        };
        let served = hyper::server::conn::http1::Builder::new()
            .auto_date_header(false)
            .header_read_timeout(None)
            .max_buf_size(MAX_HEAD)
            .serve_connection(TokioIo::new(watched), service);
        drive(served, |c| c.graceful_shutdown(), timers, stop, abort).await;
    }
}

/// Reads until the bytes differ from the HTTP/2 preface (`Some(false)`) or hold all of it
/// (`Some(true)`); `None` at EOF or on an error. Never reads past the preface.
async fn detect(tcp: &mut TcpStream, first: &mut Vec<u8>) -> Option<bool> {
    let mut buf = [0u8; H2_PREFACE.len()];
    loop {
        let n = tcp
            .read(&mut buf[..H2_PREFACE.len() - first.len()])
            .await
            .ok()?;
        if n == 0 {
            return None;
        }
        first.extend_from_slice(&buf[..n]);
        if !H2_PREFACE.starts_with(first) {
            return Some(false);
        }
        if first.len() == H2_PREFACE.len() {
            return Some(true);
        }
    }
}

/// The answer to every request until routing exists (#457).
fn no_route() -> Response<Empty<Bytes>> {
    let reason = Reason::NoRoute;
    let mut answer = Response::new(Empty::new());
    *answer.status_mut() = reason
        .status()
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::NOT_FOUND);
    let headers = answer.headers_mut();
    headers.insert("x-mimic-error", HeaderValue::from_static(reason.as_str()));
    headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
    answer
}

/// The phase of a connection and the timeouts its deadline follows.
struct Timers {
    phase: watch::Sender<Phase>,
    header: Duration,
    idle: Duration,
}

/// Drives `served` until it ends, the deadline of its phase passes (dropped), the abort
/// (dropped) or the end of the grace; at the stop it is shut down gracefully, once.
async fn drive<F: Future>(
    served: F,
    graceful: impl FnOnce(Pin<&mut F>),
    timers: Timers,
    mut stop: watch::Receiver<bool>,
    mut abort: watch::Receiver<bool>,
) {
    let mut served = pin!(served);
    let mut graceful = Some(graceful);
    let mut phase = timers.phase.subscribe();
    loop {
        let due = deadline(&phase.borrow_and_update(), timers.header, timers.idle);
        let expired = async move {
            match due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => pending().await,
            }
        };
        tokio::select! {
            _ = served.as_mut() => return,
            // The phase moved: a new deadline.
            _ = phase.changed() => {}
            () = expired => {
                let now = deadline(&phase.borrow(), timers.header, timers.idle);
                if now.is_some_and(|due| due <= Instant::now()) {
                    return;
                }
            }
            _ = stop.wait_for(|stop| *stop), if graceful.is_some() => {
                if let Some(graceful) = graceful.take() {
                    graceful(served.as_mut());
                }
            }
            _ = abort.wait_for(|abort| *abort) => return,
        }
    }
}

/// The HTTP/1 stream of a connection: a read that returns bytes while it is idle starts the
/// time allowed for the next head.
struct Watched {
    inner: Stream,
    phase: watch::Sender<Phase>,
}

impl AsyncRead for Watched {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let read = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(read, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.phase.send_if_modified(|p| match p {
                Phase::Idle(_) => {
                    *p = Phase::Head(Instant::now());
                    true
                }
                _ => false,
            });
        }
        read
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gw_19_deadlines_follow_the_connection_phase() {
        let t = Instant::now();
        let (header, idle) = (Duration::from_secs(1), Duration::from_secs(2));
        assert_eq!(deadline(&Phase::Head(t), header, idle), Some(t + header));
        assert_eq!(deadline(&Phase::Idle(t), header, idle), Some(t + idle));
        assert_eq!(deadline(&Phase::Busy(2), header, idle), None);
    }
}
