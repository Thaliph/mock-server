//! Pools of connections to pods (spec §7.9 "Load balancing", design §3.2): one pool per
//! (pod address, protocol). HTTP/1 connections carry one request at a time, up to
//! [`MAX_H1_CONNECTIONS`] per pod; HTTP/2 connections are multiplexed and a new one is opened
//! when none has a free stream, up to [`MAX_H2_CONNECTIONS`]. A connection idle for the idle
//! timeout is closed; the pool of an endpoint that left the view is drained.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::client::conn::{http1, http2};
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use super::replay::Attempt;
use crate::tls::intercept::Origin;
use crate::transport::Stream;

/// HTTP/1 connections per pod (design §3.2, the default of `circuitBreaker.maxConnections`).
pub(crate) const MAX_H1_CONNECTIONS: usize = 1024;
/// HTTP/2 connections per pod.
pub(crate) const MAX_H2_CONNECTIONS: usize = 16;
/// Pools per pod address: the TLS name of a pool is chosen by the app, so the pools of one pod
/// are bounded as its connections are.
pub(crate) const MAX_POOLS_PER_POD: usize = 8;
/// How long a connection just handed back may take to be ready for its next request.
const READY_WAIT: Duration = Duration::from_secs(1);

/// The protocol the app spoke: the pool never upgrades one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Protocol {
    Http1,
    Http2,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Connects to `addr` within `timeout` and sets the socket options of spec §4.1.
pub(crate) async fn dial(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let stream = match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(connected) => connected?,
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timeout")),
    };
    if let Err(e) = crate::connection::configure_socket(&stream) {
        tracing::debug!(upstream = %addr, error = %e, "cannot configure socket");
    }
    Ok(stream)
}

/// A connection to a pod: plain TCP, or TLS when the app's TLS is re-originated (spec §7.9).
pub(crate) enum Wire {
    Plain(TcpStream),
    Tls(Stream),
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(tcp) => Pin::new(tcp).poll_read(cx, buf),
            Wire::Tls(tls) => Pin::new(tls).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Wire::Plain(tcp) => Pin::new(tcp).poll_write(cx, buf),
            Wire::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(tcp) => Pin::new(tcp).poll_flush(cx),
            Wire::Tls(tls) => Pin::new(tls).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Wire::Plain(tcp) => Pin::new(tcp).poll_shutdown(cx),
            Wire::Tls(tls) => Pin::new(tls).poll_shutdown(cx),
        }
    }
}

/// [`dial`], then the TLS handshake of `origin` (the same timeout) when there is one: the
/// certificate is verified against its name, and a failure is an error, never cleartext.
async fn dial_wire(
    addr: SocketAddr,
    timeout: Duration,
    origin: Option<&Origin>,
) -> io::Result<Wire> {
    let tcp = dial(addr, timeout).await?;
    match origin {
        None => Ok(Wire::Plain(tcp)),
        Some(origin) => Ok(Wire::Tls(Stream::tls(
            origin.connect(tcp, timeout).await?.into(),
        ))),
    }
}

/// A connection taken from a pool for one request; handed back when dropped.
pub(crate) enum Lease {
    Http1 {
        sender: Option<http1::SendRequest<Attempt>>,
        pool: Arc<H1Pool>,
    },
    Http2 {
        sender: http2::SendRequest<Attempt>,
        active: Arc<AtomicUsize>,
        last_used: Arc<AtomicU64>,
        since: Instant,
        /// The last-stream-id of the `GOAWAY` the pod sent (`u32::MAX` while none came).
        goaway: Arc<AtomicU32>,
    },
}

impl Lease {
    /// The last-stream-id of a `GOAWAY` the pod sent on this connection (HTTP/2).
    pub(crate) fn goaway(&self) -> Option<u32> {
        match self {
            Lease::Http2 { goaway, .. } => {
                Some(goaway.load(Ordering::Acquire)).filter(|id| *id != u32::MAX)
            }
            Lease::Http1 { .. } => None,
        }
    }

    /// What tells this connection from another of its pool, for a stream sent again that must
    /// not take the one that failed (HTTP/2 only; 0 for HTTP/1, which opens a new one).
    pub(crate) fn id(&self) -> usize {
        match self {
            Lease::Http1 { .. } => 0,
            Lease::Http2 { active, .. } => Arc::as_ptr(active) as usize,
        }
    }

    /// Sends the request. A failed HTTP/1 connection is not handed back.
    pub(crate) async fn send(&mut self, req: Request<Attempt>) -> Sent {
        let sent = match self {
            Lease::Http1 { sender, .. } => {
                let Some(live) = sender.as_mut() else {
                    unreachable!("a lease sends once");
                };
                live.try_send_request(req).await
            }
            Lease::Http2 { sender, .. } => sender.try_send_request(req).await,
        };
        match sent {
            Ok(response) => Sent::Response(response),
            Err(mut failed) => {
                self.discard();
                match failed.take_message() {
                    Some(_) => Sent::Unsent,
                    None => Sent::Failed(failed.into_error()),
                }
            }
        }
    }

    /// The connection is not handed back to the pool (it failed, or became a tunnel).
    pub(crate) fn discard(&mut self) {
        if let Lease::Http1 { sender, .. } = self {
            sender.take();
        }
    }
}

/// How a request that was written failed before any response head (spec §7.9 "Retries"): a
/// stream the pod refused (`REFUSED_STREAM`, or a `GOAWAY` that left it unprocessed), a head
/// that does not parse (the pod did answer), or a reset.
///
/// `goaway` is the last-stream-id of a `GOAWAY` the pod sent on the connection, when it did. `h2`
/// gives a remote `GOAWAY` error to the streams above that id and also to every stream when the
/// connection later fails, and the error does not say which; only an id of 0 leaves no doubt (no
/// stream was processed), any other is a reset, retried by the rule of idempotent methods.
pub(crate) fn failure(e: &hyper::Error, goaway: Option<u32>) -> mimic_core::mesh::retry::Failure {
    use mimic_core::mesh::retry::Failure;
    if e.is_parse() || e.is_parse_status() || e.is_parse_too_large() {
        return Failure::Response;
    }
    let mut cause = std::error::Error::source(e);
    while let Some(err) = cause {
        if let Some(h2) = err.downcast_ref::<h2::Error>() {
            let refused = h2.reason() == Some(h2::Reason::REFUSED_STREAM)
                && h2.is_remote()
                && !h2.is_go_away()
                || h2.is_go_away() && h2.is_remote() && goaway == Some(0);
            return if refused {
                Failure::RefusedStream
            } else {
                Failure::Reset
            };
        }
        cause = err.source();
    }
    Failure::Reset
}

/// What came of sending a request.
pub(crate) enum Sent {
    Response(Response<hyper::body::Incoming>),
    /// Nothing of the request was written.
    Unsent,
    /// The request was written (or partly) and no response came.
    Failed(hyper::Error),
}

impl Drop for Lease {
    fn drop(&mut self) {
        match self {
            Lease::Http1 { sender, pool } => {
                if let Some(sender) = sender.take() {
                    pool.give_back(sender);
                }
            }
            Lease::Http2 {
                active,
                last_used,
                since,
                ..
            } => {
                active.fetch_sub(1, Ordering::AcqRel);
                last_used.store(
                    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX),
                    Ordering::Release,
                );
            }
        }
    }
}

/// A slot of [`MAX_H1_CONNECTIONS`] taken for a connection being opened; given back when
/// dropped, unless the driver task of the connection took it over (then it is forgotten).
struct Slot<'a>(&'a H1Pool);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::AcqRel);
        self.0.freed.notify_one();
    }
}

struct Idle {
    sender: http1::SendRequest<Attempt>,
    since: Instant,
}

pub(crate) struct H1Pool {
    addr: SocketAddr,
    origin: Option<Arc<Origin>>,
    idle_timeout: Duration,
    connect_timeout: Duration,
    idle: Mutex<Vec<Idle>>,
    /// Connections open, idle or not.
    open: AtomicUsize,
    freed: Notify,
    draining: AtomicBool,
}

impl H1Pool {
    fn new(
        addr: SocketAddr,
        origin: Option<Arc<Origin>>,
        idle_timeout: Duration,
        connect_timeout: Duration,
    ) -> H1Pool {
        H1Pool {
            addr,
            origin,
            idle_timeout,
            connect_timeout,
            idle: Mutex::new(Vec::new()),
            open: AtomicUsize::new(0),
            freed: Notify::new(),
            draining: AtomicBool::new(false),
        }
    }

    fn give_back(&self, sender: http1::SendRequest<Attempt>) {
        if self.draining.load(Ordering::Acquire) || sender.is_closed() {
            return;
        }
        lock(&self.idle).push(Idle {
            sender,
            since: Instant::now(),
        });
        self.freed.notify_one();
    }

    fn take_idle(&self) -> Option<http1::SendRequest<Attempt>> {
        let mut idle = lock(&self.idle);
        // The most recently used first: the others age out.
        while let Some(entry) = idle.pop() {
            if !entry.sender.is_closed() && entry.since.elapsed() < self.idle_timeout {
                return Some(entry.sender);
            }
        }
        None
    }

    async fn checkout(self: &Arc<Self>, fresh: bool) -> io::Result<Lease> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            // A reconnect (`fresh`) leaves the healthy idle connections alone.
            while !fresh && let Some(mut sender) = self.take_idle() {
                if let Ok(Ok(())) = tokio::time::timeout(READY_WAIT, sender.ready()).await {
                    return Ok(self.lease(sender));
                }
            }
            if self.open.fetch_add(1, Ordering::AcqRel) < MAX_H1_CONNECTIONS {
                // Dropped with the checkout (cancelled while connecting) or on an error, the
                // guard gives the slot back; the driver task owns it once there is one.
                let slot = Slot(self);
                let sender = self.connect(slot).await?;
                return Ok(self.lease(sender));
            }
            self.open.fetch_sub(1, Ordering::AcqRel);
            freed.await;
        }
    }

    /// A lease on a connection whose driver is gone (test hook).
    async fn closed_lease(self: &Arc<Self>) -> io::Result<Lease> {
        let stream = dial_wire(self.addr, self.connect_timeout, self.origin.as_deref()).await?;
        let (sender, connection) = http1::Builder::new()
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        drop(connection);
        Ok(self.lease(sender))
    }

    fn lease(self: &Arc<Self>, sender: http1::SendRequest<Attempt>) -> Lease {
        Lease::Http1 {
            sender: Some(sender),
            pool: self.clone(),
        }
    }

    async fn connect(self: &Arc<Self>, slot: Slot<'_>) -> io::Result<http1::SendRequest<Attempt>> {
        let stream = dial_wire(self.addr, self.connect_timeout, self.origin.as_deref()).await?;
        let (sender, connection) = http1::Builder::new()
            .preserve_header_case(true)
            .allow_obsolete_multiline_headers_in_responses(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        let pool = self.clone();
        std::mem::forget(slot);
        tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
            pool.open.fetch_sub(1, Ordering::AcqRel);
            pool.freed.notify_one();
        });
        Ok(sender)
    }

    fn sweep(&self) {
        lock(&self.idle).retain(|e| !e.sender.is_closed() && e.since.elapsed() < self.idle_timeout);
    }

    fn drain(&self) {
        self.draining.store(true, Ordering::Release);
        lock(&self.idle).clear();
    }

    /// Closes the idle connections; the pool stays in use.
    fn close_idle(&self) {
        lock(&self.idle).clear();
    }
}

struct Conn {
    sender: http2::SendRequest<Attempt>,
    /// Requests in flight on it.
    active: Arc<AtomicUsize>,
    /// Milliseconds since the pool was created, at the last request that ended.
    last_used: Arc<AtomicU64>,
    /// The peer's `MAX_CONCURRENT_STREAMS`, as it announced it (`u32::MAX` until it does).
    max_streams: Arc<AtomicU32>,
    /// The last-stream-id of the peer's `GOAWAY` (`u32::MAX` until one comes).
    goaway: Arc<AtomicU32>,
}

pub(crate) struct H2Pool {
    addr: SocketAddr,
    origin: Option<Arc<Origin>>,
    idle_timeout: Duration,
    connect_timeout: Duration,
    started: Instant,
    conns: Mutex<Vec<Conn>>,
    /// One connection is opened at a time, so that streams opened together share it.
    connecting: tokio::sync::Mutex<()>,
    draining: AtomicBool,
}

impl H2Pool {
    fn new(
        addr: SocketAddr,
        origin: Option<Arc<Origin>>,
        idle_timeout: Duration,
        connect_timeout: Duration,
    ) -> H2Pool {
        H2Pool {
            addr,
            origin,
            idle_timeout,
            connect_timeout,
            started: Instant::now(),
            conns: Mutex::new(Vec::new()),
            connecting: tokio::sync::Mutex::new(()),
            draining: AtomicBool::new(false),
        }
    }

    /// A lease on a connection whose driver is gone (test hook).
    async fn closed_lease(&self) -> io::Result<Lease> {
        let stream = dial_wire(self.addr, self.connect_timeout, self.origin.as_deref()).await?;
        let (sender, connection) = http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        drop(connection);
        Ok(Lease::Http2 {
            sender,
            active: Arc::new(AtomicUsize::new(1)),
            last_used: Arc::new(AtomicU64::new(0)),
            since: self.started,
            goaway: Arc::new(AtomicU32::new(u32::MAX)),
        })
    }

    fn lease(&self, conn: &Conn) -> Lease {
        conn.active.fetch_add(1, Ordering::AcqRel);
        Lease::Http2 {
            sender: conn.sender.clone(),
            active: conn.active.clone(),
            last_used: conn.last_used.clone(),
            since: self.started,
            goaway: conn.goaway.clone(),
        }
    }

    /// A connection with a free stream.
    fn free(&self) -> Option<Lease> {
        let mut conns = lock(&self.conns);
        conns.retain(|c| !c.sender.is_closed());
        conns
            .iter()
            .find(|c| {
                c.active.load(Ordering::Acquire) < c.max_streams.load(Ordering::Acquire) as usize
            })
            .map(|c| self.lease(c))
    }

    /// A connection with a free stream; with `again` (the id of the connection that failed) a
    /// new one, or at the bound the least busy open one that is not the failed one.
    async fn checkout(&self, again: Option<usize>) -> io::Result<Lease> {
        let fresh = again.is_some();
        if let Some(lease) = self.free().filter(|_| !fresh) {
            return Ok(lease);
        }
        let _opening = self.connecting.lock().await;
        if let Some(lease) = self.free().filter(|_| !fresh) {
            return Ok(lease);
        }
        if lock(&self.conns).len() < MAX_H2_CONNECTIONS {
            let stream = dial_wire(self.addr, self.connect_timeout, self.origin.as_deref()).await?;
            let max_streams = Arc::new(AtomicU32::new(u32::MAX));
            let goaway = Arc::new(AtomicU32::new(u32::MAX));
            let snoop = Snoop::new(stream, max_streams.clone()).with_goaway(goaway.clone());
            let (sender, connection) = http2::Builder::new(TokioExecutor::new())
                .handshake(TokioIo::new(snoop))
                .await
                .map_err(io::Error::other)?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            let conn = Conn {
                sender,
                active: Arc::new(AtomicUsize::new(0)),
                last_used: Arc::new(AtomicU64::new(0)),
                max_streams,
                goaway,
            };
            let lease = self.lease(&conn);
            lock(&self.conns).push(conn);
            return Ok(lease);
        }
        // Every connection is full and the pod is at its bound: the stream waits for a slot
        // on the least busy connection (never the one that failed).
        let conns = lock(&self.conns);
        let view: Vec<(usize, usize, bool)> = conns
            .iter()
            .map(|c| {
                (
                    Arc::as_ptr(&c.active) as usize,
                    c.active.load(Ordering::Acquire),
                    c.sender.is_closed(),
                )
            })
            .collect();
        least_busy(&view, again)
            .map(|i| self.lease(&conns[i]))
            .ok_or_else(|| io::Error::other("no connection"))
    }

    fn sweep(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let idle = u64::try_from(self.idle_timeout.as_millis()).unwrap_or(u64::MAX);
        lock(&self.conns).retain(|c| {
            !c.sender.is_closed()
                && (c.active.load(Ordering::Acquire) > 0
                    || now.saturating_sub(c.last_used.load(Ordering::Acquire)) < idle)
        });
    }

    fn drain(&self) {
        self.draining.store(true, Ordering::Release);
        lock(&self.conns).clear();
    }
}

/// The index of the open connection with the fewest requests in flight, never the one `avoid`
/// names (a resent stream must not take the connection that just failed). Each entry is
/// `(id, in flight, closed)`.
fn least_busy(conns: &[(usize, usize, bool)], avoid: Option<usize>) -> Option<usize> {
    conns
        .iter()
        .enumerate()
        .filter(|(_, (id, _, closed))| !closed && Some(*id) != avoid)
        .min_by_key(|(_, (_, active, _))| *active)
        .map(|(i, _)| i)
}

/// The largest SETTINGS payload read for `MAX_CONCURRENT_STREAMS`.
const MAX_SETTINGS_PAYLOAD: usize = 4096;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;

/// A client socket that reads the SETTINGS frames of the server to learn its
/// `MAX_CONCURRENT_STREAMS`, which `hyper` does not expose. The bytes pass unchanged.
struct Snoop {
    inner: Wire,
    max_streams: Arc<AtomicU32>,
    head: [u8; 9],
    head_len: usize,
    remaining: usize,
    /// The frame in progress is a SETTINGS frame (not an acknowledgement) of a readable size.
    settings: bool,
    /// The frame in progress is a GOAWAY frame.
    goaway_frame: bool,
    /// Where the last-stream-id of a GOAWAY goes.
    goaway: Arc<AtomicU32>,
    payload: Vec<u8>,
}

impl Snoop {
    /// Also reads the last-stream-id of the GOAWAY frames into `goaway`.
    fn with_goaway(mut self, goaway: Arc<AtomicU32>) -> Snoop {
        self.goaway = goaway;
        self
    }

    fn new(inner: Wire, max_streams: Arc<AtomicU32>) -> Snoop {
        Snoop {
            inner,
            max_streams,
            head: [0; 9],
            head_len: 0,
            remaining: 0,
            settings: false,
            goaway_frame: false,
            goaway: Arc::new(AtomicU32::new(u32::MAX)),
            payload: Vec::new(),
        }
    }

    fn feed(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.head_len < self.head.len() {
                let n = (self.head.len() - self.head_len).min(data.len());
                self.head[self.head_len..self.head_len + n].copy_from_slice(&data[..n]);
                self.head_len += n;
                data = &data[n..];
                if self.head_len < self.head.len() {
                    return;
                }
                self.remaining = usize::from(self.head[0]) << 16
                    | usize::from(self.head[1]) << 8
                    | usize::from(self.head[2]);
                self.settings = self.head[3] == 0x4
                    && self.head[4] & 0x1 == 0
                    && self.remaining <= MAX_SETTINGS_PAYLOAD;
                self.goaway_frame = self.head[3] == 0x7 && self.remaining >= 4;
                self.payload.clear();
            } else {
                let n = self.remaining.min(data.len());
                if self.settings || self.goaway_frame && self.payload.len() < 4 {
                    let take = if self.settings {
                        n
                    } else {
                        n.min(4 - self.payload.len())
                    };
                    self.payload.extend_from_slice(&data[..take]);
                }
                self.remaining -= n;
                data = &data[n..];
            }
            if self.head_len == self.head.len() && self.remaining == 0 {
                if self.settings {
                    for setting in self.payload.as_chunks::<6>().0 {
                        let id = u16::from_be_bytes([setting[0], setting[1]]);
                        if id == SETTINGS_MAX_CONCURRENT_STREAMS {
                            let value = u32::from_be_bytes([
                                setting[2], setting[3], setting[4], setting[5],
                            ]);
                            self.max_streams.store(value, Ordering::Release);
                        }
                    }
                }
                if self.goaway_frame
                    && let Some(id) = self.payload.first_chunk::<4>()
                {
                    let id = u32::from_be_bytes(*id) & 0x7fff_ffff;
                    self.goaway.fetch_min(id, Ordering::AcqRel);
                }
                self.head_len = 0;
                self.settings = false;
                self.goaway_frame = false;
            }
        }
    }
}

impl AsyncRead for Snoop {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            this.feed(&buf.filled()[before..]);
        }
        polled
    }
}

impl AsyncWrite for Snoop {
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

enum Pool {
    Http1(Arc<H1Pool>),
    Http2(Arc<H2Pool>),
}

impl Pool {
    fn sweep(&self) {
        match self {
            Pool::Http1(pool) => pool.sweep(),
            Pool::Http2(pool) => pool.sweep(),
        }
    }

    /// No connection, open or being opened.
    fn is_empty(&self) -> bool {
        match self {
            Pool::Http1(pool) => pool.open.load(Ordering::Acquire) == 0,
            Pool::Http2(pool) => lock(&pool.conns).is_empty(),
        }
    }

    fn drain(&self) {
        match self {
            Pool::Http1(pool) => pool.drain(),
            Pool::Http2(pool) => pool.drain(),
        }
    }
}

/// The pools of the sidecar.
/// A pool is per pod address, protocol and, for re-originated TLS, the name verified.
type PoolKey = (SocketAddr, Protocol, Option<Arc<str>>);

pub(crate) struct Pools {
    idle_timeout: Duration,
    pools: Mutex<HashMap<PoolKey, Arc<Pool>>>,
    sweeping: AtomicBool,
    /// Checkouts left that hand out a closed connection (test hook).
    poison: AtomicUsize,
    /// Fresh checkouts left that are refused (test hook).
    refuse: AtomicUsize,
}

impl Pools {
    pub(crate) fn new(idle_timeout: Duration) -> Arc<Pools> {
        Arc::new(Pools {
            idle_timeout,
            pools: Mutex::new(HashMap::new()),
            sweeping: AtomicBool::new(false),
            poison: AtomicUsize::new(0),
            refuse: AtomicUsize::new(0),
        })
    }

    /// A connection to `addr` for one request, from its pool or a new one. `again` is the lease
    /// of the connection a request could not be written to: a new connection, never that one.
    /// The pool of an address is created with `connect_timeout`.
    pub(crate) async fn checkout(
        self: &Arc<Self>,
        addr: SocketAddr,
        protocol: Protocol,
        origin: Option<&Arc<Origin>>,
        again: Option<&Lease>,
        connect_timeout: Duration,
    ) -> io::Result<Lease> {
        let fresh = again.is_some();
        let again = again.map(Lease::id);
        self.ensure_sweeper();
        let key = (addr, protocol, origin.map(|o| o.key().clone()));
        let pool = {
            let mut pools = lock(&self.pools);
            if let Some(pool) = pools.get(&key) {
                pool.clone()
            } else {
                if pools.keys().filter(|(at, ..)| *at == addr).count() >= MAX_POOLS_PER_POD {
                    return Err(io::Error::other("too many pools for the pod"));
                }
                let origin = origin.cloned();
                let pool = Arc::new(match protocol {
                    Protocol::Http1 => Pool::Http1(Arc::new(H1Pool::new(
                        addr,
                        origin,
                        self.idle_timeout,
                        connect_timeout,
                    ))),
                    Protocol::Http2 => Pool::Http2(Arc::new(H2Pool::new(
                        addr,
                        origin,
                        self.idle_timeout,
                        connect_timeout,
                    ))),
                });
                pools.insert(key, pool.clone());
                pool
            }
        };
        if fresh
            && self
                .refuse
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
        }
        let poisoned = !fresh
            && self
                .poison
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok();
        if poisoned {
            return match &*pool {
                Pool::Http1(pool) => pool.closed_lease().await,
                Pool::Http2(pool) => pool.closed_lease().await,
            };
        }
        match &*pool {
            Pool::Http1(pool) => pool.checkout(fresh).await,
            Pool::Http2(pool) => pool.checkout(again).await,
        }
    }

    /// The next fresh checkout fails as a refused connection would (test hook).
    pub(crate) fn refuse_next_fresh(&self) {
        self.refuse.fetch_add(1, Ordering::AcqRel);
    }

    /// The next checkout hands out a connection that is already closed.
    pub(crate) fn poison_next(&self) {
        self.poison.fetch_add(1, Ordering::AcqRel);
    }

    /// Drains the pools of the endpoints that are not in `live`: their requests in flight
    /// finish, their idle connections close.
    pub(crate) fn retain(&self, live: &HashSet<SocketAddr>) {
        let mut pools = lock(&self.pools);
        pools.retain(|(addr, ..), pool| {
            let keep = live.contains(addr);
            if !keep {
                pool.drain();
            }
            keep
        });
    }

    /// Closes the idle HTTP/1 connections to `addr` (a connection of the app left `hyper` for
    /// it, spec §4.1 limit 8).
    pub(crate) fn close_idle(&self, addr: SocketAddr) {
        for ((at, protocol, _), pool) in lock(&self.pools).iter() {
            if *at == addr
                && *protocol == Protocol::Http1
                && let Pool::Http1(pool) = &**pool
            {
                pool.close_idle();
            }
        }
    }

    /// Closes the connections idle for too long and drops the pools left with none.
    pub(crate) fn sweep(&self) {
        let pools: Vec<Arc<Pool>> = lock(&self.pools).values().cloned().collect();
        for pool in pools {
            pool.sweep();
        }
        lock(&self.pools).retain(|_, pool| {
            let empty = pool.is_empty();
            if empty {
                pool.drain();
            }
            !empty
        });
    }

    /// Starts the task that sweeps idle connections, once, on the first pool.
    fn ensure_sweeper(self: &Arc<Self>) {
        if self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let period =
            (self.idle_timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(10));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(pools) = weak.upgrade() else {
                    return;
                };
                pools.sweep();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use std::future::Future;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Lease, Pool, Pools, Protocol, Snoop, Wire, least_busy};
    use crate::tls::intercept::Origin;

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// A listener that accepts connections, holds them, and counts them; with `close` it closes
    /// them at once.
    async fn pod(close: bool) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::AcqRel);
                if !close {
                    held.push(stream);
                }
            }
        });
        (addr, accepted)
    }

    fn open_of(pools: &Pools) -> usize {
        pools
            .pools
            .lock()
            .unwrap()
            .values()
            .map(|p| match &**p {
                Pool::Http1(pool) => pool.open.load(Ordering::Acquire),
                Pool::Http2(_) => 0,
            })
            .sum()
    }

    #[tokio::test]
    async fn msh_76_a_checkout_cancelled_while_connecting_gives_its_slot_back() {
        let (addr, _) = pod(false).await;
        let pools = Pools::new(Duration::from_secs(60));
        let mut cancelled = 0;
        for _ in 0..50 {
            let before = open_of(&pools);
            let mut checkout = Box::pin(pools.checkout(addr, Protocol::Http1, None, None, TIMEOUT));
            // One poll starts the connect; a connect still pending is then cancelled.
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(checkout.as_mut().poll(cx).is_pending())
            })
            .await;
            if pending {
                drop(checkout);
                cancelled += 1;
                assert_eq!(open_of(&pools), before, "a cancelled checkout holds a slot");
            }
        }
        assert!(cancelled > 0, "no checkout was cancelled while connecting");
        assert!(
            pools
                .checkout(addr, Protocol::Http1, None, None, TIMEOUT)
                .await
                .is_ok(),
            "the next checkout"
        );
    }

    #[tokio::test]
    async fn msh_77_a_pod_has_at_most_8_pools_and_empty_ones_are_swept() {
        let (addr, _) = pod(true).await;
        let pools = Pools::new(Duration::from_secs(60));
        let mut last = None;
        for n in 0..9 {
            let origin = Arc::new(Origin::for_test(&format!("n{n}.shop.svc")));
            last = Some(
                pools
                    .checkout(addr, Protocol::Http1, Some(&origin), None, TIMEOUT)
                    .await
                    .err()
                    .expect("the pod closes every handshake"),
            );
        }
        assert_eq!(pools.pools.lock().unwrap().len(), 8, "pools of the pod");
        let ninth = last.unwrap().to_string();
        assert!(ninth.contains("too many pools"), "{ninth}");
        // A pool with a connection stays, the empty ones go and their names can be used again.
        let (plain, _) = pod(false).await;
        let lease = pools
            .checkout(plain, Protocol::Http1, None, None, TIMEOUT)
            .await
            .unwrap();
        drop(lease);
        pools.sweep();
        assert_eq!(pools.pools.lock().unwrap().len(), 1, "after the sweep");
        let origin = Arc::new(Origin::for_test("n8.shop.svc"));
        let again = pools
            .checkout(addr, Protocol::Http1, Some(&origin), None, TIMEOUT)
            .await
            .err()
            .expect("the pod closes every handshake")
            .to_string();
        assert!(!again.contains("too many pools"), "{again}");
    }

    async fn get(pools: &Arc<Pools>, addr: SocketAddr, again: Option<&Lease>) -> Lease {
        pools
            .checkout(addr, Protocol::Http1, None, again, TIMEOUT)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn msh_78_a_fresh_checkout_keeps_a_healthy_idle_connection() {
        let (addr, accepted) = pod(false).await;
        let pools = Pools::new(Duration::from_secs(60));
        let mut failed = get(&pools, addr, None).await;
        let idle = get(&pools, addr, None).await;
        drop(idle);
        // The reconnect after the failed one opens a third connection and leaves the idle one.
        failed.discard();
        let fresh = get(&pools, addr, Some(&failed)).await;
        drop(fresh);
        drop(failed);
        let (_a, _b) = (get(&pools, addr, None).await, get(&pools, addr, None).await);
        // The pod counts a connection when it accepts it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(accepted.load(Ordering::Acquire), 3, "connections accepted");
    }

    #[test]
    fn msh_69_at_the_h2_bound_a_resent_stream_never_takes_the_failed_connection() {
        // (id, requests in flight, closed)
        let conns = [(1usize, 0usize, false), (2, 3, false), (3, 1, true)];
        // No connection to avoid: the least busy one that is open.
        assert_eq!(least_busy(&conns, None), Some(0));
        // The least busy one failed: the next, never the failed one, never a closed one.
        assert_eq!(least_busy(&conns, Some(1)), Some(1));
        // Only the failed connection is left.
        assert_eq!(least_busy(&conns[..1], Some(1)), None);
        assert_eq!(least_busy(&[], None), None);
    }

    async fn snoop(max: &Arc<AtomicU32>) -> Snoop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        Snoop::new(Wire::Plain(stream), max.clone())
    }

    fn settings(values: &[(u16, u32)], flags: u8) -> Vec<u8> {
        let len = values.len() * 6;
        let mut frame = vec![
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
            0x4,
            flags,
            0,
            0,
            0,
            0,
        ];
        for (id, value) in values {
            frame.extend_from_slice(&id.to_be_bytes());
            frame.extend_from_slice(&value.to_be_bytes());
        }
        frame
    }

    #[tokio::test]
    async fn msh_50_the_max_concurrent_streams_is_read_from_settings_frames_however_they_are_cut() {
        let max = Arc::new(AtomicU32::new(u32::MAX));
        let mut stream = snoop(&max).await;
        // A frame of another type first, then SETTINGS with other settings around the one.
        let mut bytes = vec![0, 0, 3, 0x6, 0, 0, 0, 0, 0, 1, 2, 3];
        bytes.extend(settings(&[(1, 4096), (3, 7), (4, 65535)], 0));
        for chunk in bytes.chunks(1) {
            stream.feed(chunk);
        }
        assert_eq!(max.load(Ordering::Acquire), 7);
        // An acknowledgement carries none, and a later SETTINGS changes it.
        stream.feed(&settings(&[], 0x1));
        assert_eq!(max.load(Ordering::Acquire), 7);
        stream.feed(&settings(&[(3, 100)], 0));
        assert_eq!(max.load(Ordering::Acquire), 100);
        // Several frames in one read.
        let mut two = settings(&[(3, 5)], 0);
        two.extend(settings(&[(3, 6)], 0));
        stream.feed(&two);
        assert_eq!(max.load(Ordering::Acquire), 6);
    }
}
