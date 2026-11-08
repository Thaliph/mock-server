//! The HTTP/2 relay (spec §4.1), prior knowledge (`h2c`, gRPC included): a `hyper` server
//! connection on the client socket and a `hyper` client connection on the destination socket,
//! one for the other, with no pool and no timer. Requests are forwarded as received; the proxy
//! never routes on `Host` and never answers for a destination failure. HTTP/1 goes through the
//! codec ([`crate::http1`]). Each side is a plain TCP or a TLS [`Stream`] (the h2 streams are
//! then the decrypted ones).

use std::future::Future;
use std::io;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Request, Response, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::discovery::Tap;
use crate::exchange::{self, Ctx, ReqBody, Tracker};
use crate::load::{Body, Marker, mark_answer, refused, screen};
use crate::mock::{self, Mock};
use crate::transport::Stream;

const PROTOCOL: &str = "h2";

/// The client socket, replaying the bytes consumed by protocol detection first.
struct Replay {
    prefix: Vec<u8>,
    pos: usize,
    inner: Stream,
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

/// What the HTTP/2 service shares with [`relay`]: the destination connection, on which all
/// the streams of the client connection are multiplexed.
struct State {
    ctx: Ctx,
    tap: Option<Tap>,
    /// Taken when the connection switches to mock (spec §8 "Switching"): the destination
    /// connection ends once the streams in flight have.
    sender: Mutex<Option<hyper::client::conn::http2::SendRequest<ReqBody>>>,
    /// Outbound discovery connections only: the mock that takes the streams opened after a
    /// switch to `mock`.
    mock: Option<Mock>,
    mocked: OnceLock<Arc<mock::Conn>>,
    /// The destination connection was let go on purpose: its end is no failure.
    switched: AtomicBool,
    /// Inbound connections: the check of the load marker of every stream (spec §7.8).
    load: Option<Marker>,
    /// With a load check, the destination socket stays unused (not one byte written) until
    /// the first stream is forwarded: a refused stream leaves the app with nothing.
    unopened: tokio::sync::Mutex<Option<Stream>>,
    /// The lazily opened destination connection ended.
    driver_ended: Arc<tokio::sync::Notify>,
    driver_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl State {
    /// Opens the destination connection if it is not yet (the first forwarded stream).
    async fn open(&self) -> io::Result<()> {
        let mut unopened = self.unopened.lock().await;
        let Some(upstream) = unopened.take() else {
            return Ok(());
        };
        let (sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, ReqBody>(TokioIo::new(upstream))
            .await
            .map_err(io::Error::other)?;
        *self.sender.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
        let ended = self.driver_ended.clone();
        let task = tokio::spawn(async move {
            let _ = driver.await;
            ended.notify_one();
        });
        *self
            .driver_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(task);
        Ok(())
    }
}

/// Ends the lazily opened destination connection with the relay.
struct Closing(Arc<State>);

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(task) = self
            .0
            .driver_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// Relays the HTTP/2 streams of `client` (whose first bytes, `replay`, were already read) to
/// `upstream` until one side closes. A stream the destination fails is reset alone.
pub async fn relay(
    client: Stream,
    upstream: Stream,
    replay: Vec<u8>,
    ctx: Ctx,
    stop: impl Future<Output = ()>,
) -> io::Result<()> {
    relay_with(client, upstream, replay, ctx, (None, None, None), stop).await
}

/// Like [`relay`], with the exchanges also handed to the discovery `tap` (spec §6), the `mock`
/// that takes the streams opened after a switch to `mock` (outbound) and the load marker check
/// (inbound).
pub(crate) async fn relay_with(
    client: Stream,
    upstream: Stream,
    replay: Vec<u8>,
    ctx: Ctx,
    (tap, mock, load): (Option<Tap>, Option<Mock>, Option<Marker>),
    stop: impl Future<Output = ()>,
) -> io::Result<()> {
    let lazy = load.is_some();
    let (sender, driver, unopened) = if lazy {
        (None, None, Some(upstream))
    } else {
        let (sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake::<_, ReqBody>(TokioIo::new(upstream))
            .await
            .map_err(io::Error::other)?;
        (Some(sender), Some(Box::pin(driver)), None)
    };
    let state = Arc::new(State {
        ctx: ctx.clone(),
        tap,
        sender: Mutex::new(sender),
        mock,
        mocked: OnceLock::new(),
        switched: AtomicBool::new(false),
        load,
        unopened: tokio::sync::Mutex::new(unopened),
        driver_ended: Arc::default(),
        driver_task: Mutex::new(None),
    });
    let shared = state.clone();
    let _closing = Closing(state.clone());
    let left = state.mock.clone();
    let service = service_fn(move |req| forward(state.clone(), req));
    let io = TokioIo::new(Replay {
        prefix: replay,
        pos: 0,
        inner: client,
    });
    let server = server::conn::http2::Builder::new(TokioExecutor::new())
        .auto_date_header(false)
        .serve_connection(io, service);
    let mut server = pin!(server);
    // Dropped as soon as it is done: a finished client connection still holds its handle on
    // the destination socket, which would stay open.
    let mut driver = driver;

    let mut stop = pin!(stop);
    let mut stopping = false;
    let mut driver_done = false;
    let mut leaving = false;
    loop {
        if driver_done {
            driver = None;
        }
        tokio::select! {
            served = server.as_mut() => return finish(&ctx, served),
            () = &mut stop, if !stopping => {
                stopping = true;
                // Shutdown: GOAWAY, streams in flight finish.
                server.as_mut().graceful_shutdown();
            }
            () = async {
                match &left {
                    Some(mock) => mock.left().await,
                    None => std::future::pending().await,
                }
            }, if !leaving && shared.switched.load(Ordering::Acquire) => {
                leaving = true;
                // Back to discovery: the mocked streams in flight finish, then GOAWAY.
                server.as_mut().graceful_shutdown();
            }
            () = async {
                tokio::select! {
                    () = connection_done(&mut driver) => {}
                    () = shared.driver_ended.notified() => {}
                }
            }, if !driver_done => {
                driver_done = true;
                // The destination connection ended: streams in flight finish or fail, and
                // the client is told to open no new one (GOAWAY), unless it was let go of
                // because the connection continues as mocked.
                if !shared.switched.load(Ordering::Acquire) {
                    server.as_mut().graceful_shutdown();
                }
            }
        }
    }
}

/// Resolves when the destination connection ends; never when it is gone already.
async fn connection_done<F: Future + Unpin>(connection: &mut Option<F>) {
    match connection {
        Some(connection) => {
            let _ = connection.await;
        }
        None => std::future::pending().await,
    }
}

/// The end of the client connection: one the client broke at connection level (`h2` sent
/// `GOAWAY` with an error) is logged as rejected; a client that just leaves is not.
fn finish(ctx: &Ctx, served: Result<(), hyper::Error>) -> io::Result<()> {
    let Err(e) = served else {
        return Ok(());
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(err) = source {
        if let Some(h2) = err.downcast_ref::<h2::Error>()
            && h2.is_go_away()
            && !h2.is_remote()
            && h2.reason() != Some(h2::Reason::NO_ERROR)
        {
            exchange::rejected(ctx);
            break;
        }
        source = err.source();
    }
    Err(io::Error::other(e))
}

async fn forward(
    state: Arc<State>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Body>, Box<dyn std::error::Error + Send + Sync>> {
    // A stream opened after the switch to mock is never forwarded (spec §8 "Switching").
    if let Some(mock) = state.mock.as_ref().filter(|mock| mock.active()) {
        state.switched.store(true, Ordering::Release);
        state
            .sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let conn = state
            .mocked
            .get_or_init(|| mock.switched_h2(state.ctx.clone()));
        let answered = conn.serve(req).await?;
        return Ok(answered.map(|body| body.map_err(|never| match never {}).boxed_unsync()));
    }
    let mut req = req;
    // A load request is checked, loses its ticket, is not discovered, and its answer is marked.
    let load_run = match &state.load {
        Some(marker) => match screen(marker, state.tap.as_ref(), req.headers_mut()) {
            Ok(run) => run,
            Err(refusal) => return Ok(refused(refusal)),
        },
        None => None,
    };
    state.open().await?;
    let sender = state
        .sender
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let Some(mut sender) = sender else {
        // The destination connection was let go of by a switch, and the mode went back to
        // discovery since: the client opens a new connection.
        return Err("destination connection closed".into());
    };
    let tap = state.tap.as_ref().filter(|_| load_run.is_none());
    let (mut tracker, req) = Tracker::start(&state.ctx, PROTOCOL, tap, req);
    let marked = load_run.is_some();
    if let Some(uid) = load_run {
        tracker.load_run(uid);
    }
    let sent = async {
        sender.ready().await?;
        sender.send_request(req).await
    }
    .await;
    match sent {
        Ok(response) => {
            let mut answered = tracker.answered(response).map(BodyExt::boxed_unsync);
            if marked {
                mark_answer(answered.headers_mut());
            }
            Ok(answered)
        }
        Err(e) => {
            tracker.failed();
            Err(e.into())
        }
    }
}
