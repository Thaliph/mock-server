//! The HTTP/2 relay (spec §4.1), prior knowledge (`h2c`, gRPC included): a `hyper` server
//! connection on the client socket and a `hyper` client connection on the destination socket,
//! one for the other, with no pool and no timer. Requests are forwarded as received; the proxy
//! never routes on `Host` and never answers for a destination failure. HTTP/1 goes through the
//! codec ([`crate::http1`]).

use std::future::Future;
use std::io;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::service::service_fn;
use hyper::{Request, Response, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::exchange::{self, Ctx, ReqBody, RespBody, Tracker};

const PROTOCOL: &str = "h2";

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

/// What the HTTP/2 service shares with [`relay`]: the destination connection, on which all
/// the streams of the client connection are multiplexed.
struct State {
    ctx: Ctx,
    sender: hyper::client::conn::http2::SendRequest<ReqBody>,
}

/// Relays the HTTP/2 streams of `client` (whose first bytes, `replay`, were already read) to
/// `upstream` until one side closes. A stream the destination fails is reset alone.
pub async fn relay(
    client: TcpStream,
    upstream: TcpStream,
    replay: Vec<u8>,
    ctx: Ctx,
    stop: impl Future<Output = ()>,
) -> io::Result<()> {
    let (sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake::<_, ReqBody>(TokioIo::new(upstream))
        .await
        .map_err(io::Error::other)?;
    let state = Arc::new(State {
        ctx: ctx.clone(),
        sender,
    });
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
    let mut driver = pin!(driver);

    let mut stop = pin!(stop);
    let mut stopping = false;
    let mut driver_done = false;
    loop {
        tokio::select! {
            served = server.as_mut() => return finish(&ctx, served),
            () = &mut stop, if !stopping => {
                stopping = true;
                // Shutdown: GOAWAY, streams in flight finish.
                server.as_mut().graceful_shutdown();
            }
            _ = &mut driver, if !driver_done => {
                driver_done = true;
                // The destination connection ended: streams in flight finish or fail, and
                // the client is told to open no new one (GOAWAY).
                server.as_mut().graceful_shutdown();
            }
        }
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
) -> Result<Response<RespBody>, hyper::Error> {
    let (tracker, req) = Tracker::start(&state.ctx, PROTOCOL, req);
    let mut sender = state.sender.clone();
    let sent = async {
        sender.ready().await?;
        sender.send_request(req).await
    }
    .await;
    match sent {
        Ok(response) => Ok(tracker.answered(response)),
        Err(e) => {
            tracker.failed();
            Err(e)
        }
    }
}
