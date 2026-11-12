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

use crate::exchange::ReqBody;

/// HTTP/1 connections per pod (design §3.2, the default of `circuitBreaker.maxConnections`).
pub(crate) const MAX_H1_CONNECTIONS: usize = 1024;
/// HTTP/2 connections per pod.
pub(crate) const MAX_H2_CONNECTIONS: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
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

async fn dial(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(connected) => connected?,
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timeout")),
    };
    if let Err(e) = crate::connection::configure_socket(&stream) {
        tracing::debug!(upstream = %addr, error = %e, "cannot configure socket");
    }
    Ok(stream)
}

/// A connection taken from a pool for one request; handed back when dropped.
pub(crate) enum Lease {
    Http1 {
        sender: Option<http1::SendRequest<ReqBody>>,
        pool: Arc<H1Pool>,
    },
    Http2 {
        sender: http2::SendRequest<ReqBody>,
        active: Arc<AtomicUsize>,
        last_used: Arc<AtomicU64>,
        since: Instant,
    },
}

impl Lease {
    /// Sends the request. A failed HTTP/1 connection is not handed back.
    pub(crate) async fn send(&mut self, req: Request<ReqBody>) -> Sent {
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
                    Some(request) => Sent::Unsent(request),
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

/// What came of sending a request.
pub(crate) enum Sent {
    Response(Response<hyper::body::Incoming>),
    /// Nothing of the request was written: it is returned, whole.
    Unsent(Request<ReqBody>),
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

struct Idle {
    sender: http1::SendRequest<ReqBody>,
    since: Instant,
}

pub(crate) struct H1Pool {
    addr: SocketAddr,
    idle_timeout: Duration,
    idle: Mutex<Vec<Idle>>,
    /// Connections open, idle or not.
    open: AtomicUsize,
    freed: Notify,
    draining: AtomicBool,
}

impl H1Pool {
    fn new(addr: SocketAddr, idle_timeout: Duration) -> H1Pool {
        H1Pool {
            addr,
            idle_timeout,
            idle: Mutex::new(Vec::new()),
            open: AtomicUsize::new(0),
            freed: Notify::new(),
            draining: AtomicBool::new(false),
        }
    }

    fn give_back(&self, sender: http1::SendRequest<ReqBody>) {
        if self.draining.load(Ordering::Acquire) || sender.is_closed() {
            return;
        }
        lock(&self.idle).push(Idle {
            sender,
            since: Instant::now(),
        });
        self.freed.notify_one();
    }

    fn take_idle(&self) -> Option<http1::SendRequest<ReqBody>> {
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
            while let Some(mut sender) = self.take_idle().filter(|_| !fresh) {
                if let Ok(Ok(())) = tokio::time::timeout(READY_WAIT, sender.ready()).await {
                    return Ok(self.lease(sender));
                }
            }
            if self.open.fetch_add(1, Ordering::AcqRel) < MAX_H1_CONNECTIONS {
                return match self.connect().await {
                    Ok(sender) => Ok(self.lease(sender)),
                    Err(e) => {
                        self.open.fetch_sub(1, Ordering::AcqRel);
                        self.freed.notify_one();
                        Err(e)
                    }
                };
            }
            self.open.fetch_sub(1, Ordering::AcqRel);
            freed.await;
        }
    }

    fn lease(self: &Arc<Self>, sender: http1::SendRequest<ReqBody>) -> Lease {
        Lease::Http1 {
            sender: Some(sender),
            pool: self.clone(),
        }
    }

    async fn connect(self: &Arc<Self>) -> io::Result<http1::SendRequest<ReqBody>> {
        let stream = dial(self.addr).await?;
        let (sender, connection) = http1::Builder::new()
            .preserve_header_case(true)
            .allow_obsolete_multiline_headers_in_responses(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        let pool = self.clone();
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
}

struct Conn {
    sender: http2::SendRequest<ReqBody>,
    /// Requests in flight on it.
    active: Arc<AtomicUsize>,
    /// Milliseconds since the pool was created, at the last request that ended.
    last_used: Arc<AtomicU64>,
    /// The peer's `MAX_CONCURRENT_STREAMS`, as it announced it (`u32::MAX` until it does).
    max_streams: Arc<AtomicU32>,
}

pub(crate) struct H2Pool {
    addr: SocketAddr,
    idle_timeout: Duration,
    started: Instant,
    conns: Mutex<Vec<Conn>>,
    /// One connection is opened at a time, so that streams opened together share it.
    connecting: tokio::sync::Mutex<()>,
    draining: AtomicBool,
}

impl H2Pool {
    fn new(addr: SocketAddr, idle_timeout: Duration) -> H2Pool {
        H2Pool {
            addr,
            idle_timeout,
            started: Instant::now(),
            conns: Mutex::new(Vec::new()),
            connecting: tokio::sync::Mutex::new(()),
            draining: AtomicBool::new(false),
        }
    }

    fn lease(&self, conn: &Conn) -> Lease {
        conn.active.fetch_add(1, Ordering::AcqRel);
        Lease::Http2 {
            sender: conn.sender.clone(),
            active: conn.active.clone(),
            last_used: conn.last_used.clone(),
            since: self.started,
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

    async fn checkout(&self) -> io::Result<Lease> {
        if let Some(lease) = self.free() {
            return Ok(lease);
        }
        let _opening = self.connecting.lock().await;
        if let Some(lease) = self.free() {
            return Ok(lease);
        }
        if lock(&self.conns).len() < MAX_H2_CONNECTIONS {
            let stream = dial(self.addr).await?;
            let max_streams = Arc::new(AtomicU32::new(u32::MAX));
            let (sender, connection) = http2::Builder::new(TokioExecutor::new())
                .handshake(TokioIo::new(Snoop::new(stream, max_streams.clone())))
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
            };
            let lease = self.lease(&conn);
            lock(&self.conns).push(conn);
            return Ok(lease);
        }
        // Every connection is full and the pod is at its bound: the stream waits for a slot
        // on the least busy connection.
        let conns = lock(&self.conns);
        conns
            .iter()
            .min_by_key(|c| c.active.load(Ordering::Acquire))
            .map(|c| self.lease(c))
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

/// The largest SETTINGS payload read for `MAX_CONCURRENT_STREAMS`.
const MAX_SETTINGS_PAYLOAD: usize = 4096;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;

/// A client socket that reads the SETTINGS frames of the server to learn its
/// `MAX_CONCURRENT_STREAMS`, which `hyper` does not expose. The bytes pass unchanged.
struct Snoop {
    inner: TcpStream,
    max_streams: Arc<AtomicU32>,
    head: [u8; 9],
    head_len: usize,
    remaining: usize,
    /// The frame in progress is a SETTINGS frame (not an acknowledgement) of a readable size.
    settings: bool,
    payload: Vec<u8>,
}

impl Snoop {
    fn new(inner: TcpStream, max_streams: Arc<AtomicU32>) -> Snoop {
        Snoop {
            inner,
            max_streams,
            head: [0; 9],
            head_len: 0,
            remaining: 0,
            settings: false,
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
                self.payload.clear();
            } else {
                let n = self.remaining.min(data.len());
                if self.settings {
                    self.payload.extend_from_slice(&data[..n]);
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
                self.head_len = 0;
                self.settings = false;
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

    fn drain(&self) {
        match self {
            Pool::Http1(pool) => pool.drain(),
            Pool::Http2(pool) => pool.drain(),
        }
    }
}

/// The pools of the sidecar.
pub(crate) struct Pools {
    idle_timeout: Duration,
    pools: Mutex<HashMap<(SocketAddr, Protocol), Arc<Pool>>>,
    sweeping: AtomicBool,
}

impl Pools {
    pub(crate) fn new(idle_timeout: Duration) -> Arc<Pools> {
        Arc::new(Pools {
            idle_timeout,
            pools: Mutex::new(HashMap::new()),
            sweeping: AtomicBool::new(false),
        })
    }

    /// A connection to `addr` for one request, from its pool or a new one.
    pub(crate) async fn checkout(
        self: &Arc<Self>,
        addr: SocketAddr,
        protocol: Protocol,
        fresh: bool,
    ) -> io::Result<Lease> {
        self.ensure_sweeper();
        let pool = lock(&self.pools)
            .entry((addr, protocol))
            .or_insert_with(|| {
                Arc::new(match protocol {
                    Protocol::Http1 => Pool::Http1(Arc::new(H1Pool::new(addr, self.idle_timeout))),
                    Protocol::Http2 => Pool::Http2(Arc::new(H2Pool::new(addr, self.idle_timeout))),
                })
            })
            .clone();
        match &*pool {
            Pool::Http1(pool) => pool.checkout(fresh).await,
            Pool::Http2(pool) => pool.checkout().await,
        }
    }

    /// Drains the pools of the endpoints that are not in `live`: their requests in flight
    /// finish, their idle connections close.
    pub(crate) fn retain(&self, live: &HashSet<SocketAddr>) {
        let mut pools = lock(&self.pools);
        pools.retain(|(addr, _), pool| {
            let keep = live.contains(addr);
            if !keep {
                pool.drain();
            }
            keep
        });
    }

    /// Closes the connections idle for too long.
    pub(crate) fn sweep(&self) {
        let pools: Vec<Arc<Pool>> = lock(&self.pools).values().cloned().collect();
        for pool in pools {
            pool.sweep();
        }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::Snoop;

    async fn snoop(max: &Arc<AtomicU32>) -> Snoop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        Snoop::new(stream, max.clone())
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
