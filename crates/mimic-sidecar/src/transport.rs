//! One connection side, plain TCP or TLS (spec §4.1), behind a single type so that the HTTP/1
//! and h2 relays do not care. The TCP socket stays reachable ([`Stream::socket`]) for what only
//! the socket can say: a pending error (a reset) and the reset of the close (`SO_LINGER` 0).
//!
//! Close semantics over TLS: a FIN written is `close_notify` then FIN; a FIN read without
//! `close_notify` (a peer that just closes its socket) is read as a FIN, like a clean end; a
//! reset is a socket error and stays one.

use std::io;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use socket2::SockRef;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

/// A TCP socket shared by the two directions of a connection side (and by the error watch),
/// read and written by readiness, as the relays always did. It can start with bytes that were
/// already read (the ClientHello consumed by protocol detection), which the first reads return.
#[derive(Clone)]
pub struct Socket {
    tcp: Arc<TcpStream>,
    prefix: Arc<Mutex<Vec<u8>>>,
}

impl Socket {
    pub fn new(tcp: TcpStream) -> Self {
        Self::with_prefix(tcp, Vec::new())
    }

    /// A socket that reads `prefix` before anything from `tcp`.
    pub fn with_prefix(tcp: TcpStream, prefix: Vec<u8>) -> Self {
        Self {
            tcp: Arc::new(tcp),
            prefix: Arc::new(Mutex::new(prefix)),
        }
    }

    pub fn tcp(&self) -> &TcpStream {
        &self.tcp
    }
}

impl AsyncRead for Socket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        {
            let mut prefix = self.prefix.lock().unwrap_or_else(|e| e.into_inner());
            if !prefix.is_empty() {
                let n = prefix.len().min(buf.remaining());
                buf.put_slice(&prefix[..n]);
                prefix.drain(..n);
                return Poll::Ready(Ok(()));
            }
        }
        loop {
            ready!(self.tcp.poll_read_ready(cx))?;
            match self.tcp.try_read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.tcp.poll_write_ready(cx))?;
            match self.tcp.try_write(buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// The FIN; a socket that is already gone has nothing left to close.
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(
            SockRef::from(&*self.tcp)
                .shutdown(Shutdown::Write)
                .or_else(|e| match e.kind() {
                    io::ErrorKind::NotConnected => Ok(()),
                    _ => Err(e),
                }),
        )
    }
}

/// One side of a relayed connection.
pub enum Stream {
    Plain(Socket),
    Tls(Box<TlsStream<Socket>>),
}

impl Stream {
    pub fn plain(tcp: TcpStream) -> Self {
        Self::Plain(Socket::new(tcp))
    }

    pub fn tls(tls: TlsStream<Socket>) -> Self {
        Self::Tls(Box::new(tls))
    }

    /// The TCP socket under the stream.
    pub fn socket(&self) -> Socket {
        match self {
            Self::Plain(socket) => socket.clone(),
            Self::Tls(tls) => tls.get_ref().0.clone(),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(socket) => Pin::new(socket).poll_read(cx, buf),
            Self::Tls(tls) => match ready!(Pin::new(tls).poll_read(cx, buf)) {
                // A FIN without `close_notify`: the peer is done writing.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Poll::Ready(Ok(())),
                r => Poll::Ready(r),
            },
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Plain(socket) => Pin::new(socket).poll_write(cx, buf),
            Self::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(socket) => Pin::new(socket).poll_flush(cx),
            Self::Tls(tls) => Pin::new(tls).poll_flush(cx),
        }
    }

    /// A FIN; over TLS `close_notify` first.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(socket) => Pin::new(socket).poll_shutdown(cx),
            Self::Tls(tls) => Pin::new(tls).poll_shutdown(cx),
        }
    }
}
