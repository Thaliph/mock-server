//! The HTTP relays (spec §4.1): a `hyper` server connection on the client socket and a
//! `hyper` client connection on the destination socket, one for the other, with no pool and no
//! timer (so no header-read, idle or request timeout). Requests are forwarded as received;
//! the proxy never routes on `Host` and never answers for a destination failure. HTTP/1 is
//! [`relay`], HTTP/2 prior knowledge (`h2c`, gRPC included) is [`relay_h2`].

use std::io;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use hyper::service::service_fn;
use hyper::upgrade::OnUpgrade;
use hyper::{Method, Request, Response, StatusCode, header, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, copy_bidirectional};
use tokio::net::TcpStream;

use crate::exchange::{self, Ctx, ReqBody, RespBody, Tracker};

const PROTOCOL: &str = "http1";
const PROTOCOL_H2: &str = "h2";

/// More header lines than this, or a header section over [`MAX_HEADER_BYTES`], gets `431`.
const MAX_HEADERS: usize = 256;
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The client socket, replaying the bytes consumed by protocol detection first.
struct Replay {
    prefix: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl AsyncRead for Replay {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Replay {
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

/// What the service shares with [`relay`].
struct State {
    ctx: Ctx,
    sender: tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<ReqBody>>,
    /// Set when a request asks for an upgrade or is a `CONNECT`.
    upgrade_requested: AtomicBool,
    /// The two sides of an accepted upgrade: client side, destination side.
    upgraded: Mutex<Option<(OnUpgrade, OnUpgrade)>>,
}

/// Relays the HTTP/1 requests of `client` (whose first bytes, `replay`, were already read) to
/// `upstream` until one side closes. An accepted upgrade continues as an opaque tunnel.
pub async fn relay(
    client: TcpStream,
    upstream: TcpStream,
    replay: Vec<u8>,
    ctx: Ctx,
) -> io::Result<()> {
    let (sender, driver) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .handshake::<_, ReqBody>(TokioIo::new(upstream))
        .await
        .map_err(io::Error::other)?;
    let state = Arc::new(State {
        ctx,
        sender: tokio::sync::Mutex::new(sender),
        upgrade_requested: AtomicBool::new(false),
        upgraded: Mutex::new(None),
    });
    let service = {
        let state = state.clone();
        service_fn(move |req| forward(state.clone(), req))
    };
    let io = TokioIo::new(Replay {
        prefix: replay,
        pos: 0,
        inner: client,
    });
    let server = server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .auto_date_header(false)
        .half_close(true)
        .max_headers(MAX_HEADERS)
        .max_buf_size(MAX_HEADER_BYTES)
        .serve_connection(io, service)
        .with_upgrades();
    let mut server = pin!(server);
    let mut driver = pin!(driver.with_upgrades());

    let mut driver_done = false;
    let served = loop {
        tokio::select! {
            served = server.as_mut() => break served,
            _ = &mut driver, if !driver_done => {
                driver_done = true;
                // The destination closed: finish the response in flight, then close the
                // client side too (at once when it is idle).
                if !state.upgrade_requested.load(Ordering::SeqCst) {
                    server.as_mut().graceful_shutdown();
                }
            }
        }
    };
    if let Err(e) = &served
        && e.is_parse()
    {
        let status = if e.is_parse_too_large() { 431 } else { 400 };
        exchange::rejected(&state.ctx, PROTOCOL, status);
    }
    served.map_err(io::Error::other)?;

    let pending = state
        .upgraded
        .lock()
        .map_err(|_| io::Error::other("poisoned"))?
        .take();
    let Some((from_client, from_upstream)) = pending else {
        return Ok(());
    };
    let Ok(client_side) = from_client.await else {
        return Ok(());
    };
    let upstream_side = if driver_done {
        from_upstream.await
    } else {
        tokio::join!(from_upstream, &mut driver).0
    };
    let Ok(upstream_side) = upstream_side else {
        return Ok(());
    };
    copy_bidirectional(
        &mut TokioIo::new(client_side),
        &mut TokioIo::new(upstream_side),
    )
    .await
    .map(|_| ())
}

async fn forward(
    state: Arc<State>,
    mut req: Request<hyper::body::Incoming>,
) -> Result<Response<RespBody>, hyper::Error> {
    let is_connect = req.method() == Method::CONNECT;
    let wants_upgrade = is_connect || req.headers().contains_key(header::UPGRADE);
    let from_client = wants_upgrade.then(|| {
        state.upgrade_requested.store(true, Ordering::SeqCst);
        hyper::upgrade::on(&mut req)
    });
    let (tracker, req) = Tracker::start(&state.ctx, PROTOCOL, req);
    let sent = async {
        let mut sender = state.sender.lock().await;
        sender.ready().await?;
        sender.send_request(req).await
    }
    .await;
    let mut response = match sent {
        Ok(response) => response,
        Err(e) => {
            tracker.failed();
            return Err(e);
        }
    };
    let accepted = response.status() == StatusCode::SWITCHING_PROTOCOLS
        || (is_connect && response.status().is_success());
    if let (Some(from_client), true) = (from_client, accepted)
        && let Ok(mut slot) = state.upgraded.lock()
    {
        *slot = Some((from_client, hyper::upgrade::on(&mut response)));
    }
    Ok(tracker.answered(response, accepted))
}

/// What the HTTP/2 service shares with [`relay_h2`]: the destination connection, on which all
/// the streams of the client connection are multiplexed.
struct StateH2 {
    ctx: Ctx,
    sender: hyper::client::conn::http2::SendRequest<ReqBody>,
}

/// Relays the HTTP/2 streams of `client` (whose first bytes, `replay`, were already read) to
/// `upstream` until one side closes. A stream the destination fails is reset alone.
pub async fn relay_h2(
    client: TcpStream,
    upstream: TcpStream,
    replay: Vec<u8>,
    ctx: Ctx,
) -> io::Result<()> {
    let (sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, ReqBody>(TokioIo::new(upstream))
        .await
        .map_err(io::Error::other)?;
    let state = Arc::new(StateH2 { ctx, sender });
    let service = service_fn(move |req| forward_h2(state.clone(), req));
    let io = TokioIo::new(Replay {
        prefix: replay,
        pos: 0,
        inner: client,
    });
    let server = server::conn::http2::Builder::new(TokioExecutor::new())
        .auto_date_header(false)
        .serve_connection(io, service);
    let mut server = pin!(server);
    let mut driver = pin!(driver);

    let mut driver_done = false;
    loop {
        tokio::select! {
            served = server.as_mut() => return served.map_err(io::Error::other),
            _ = &mut driver, if !driver_done => {
                driver_done = true;
                // The destination connection ended: streams in flight finish or fail, and
                // the client is told to open no new one (GOAWAY).
                server.as_mut().graceful_shutdown();
            }
        }
    }
}

async fn forward_h2(
    state: Arc<StateH2>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<RespBody>, hyper::Error> {
    let (tracker, req) = Tracker::start(&state.ctx, PROTOCOL_H2, req);
    let mut sender = state.sender.clone();
    let sent = async {
        sender.ready().await?;
        sender.send_request(req).await
    }
    .await;
    match sent {
        Ok(response) => Ok(tracker.answered(response, false)),
        Err(e) => {
            tracker.failed();
            Err(e)
        }
    }
}
