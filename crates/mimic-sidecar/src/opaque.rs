//! The opaque tunnel (spec §4.1): bytes are relayed unchanged in both directions, with no idle
//! timeout; a close or half-close on one side reaches the other. The bytes relayed and the side
//! that failed first are recorded for the access line and the metrics.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, copy_bidirectional};
use tokio::net::TcpStream;

use crate::access::Outcome;

const NONE: u8 = 0;
const CLIENT: u8 = 1;
const UPSTREAM: u8 = 2;

/// How a tunnel ended.
pub struct Tunnelled {
    /// Bytes read from the client, including those read by detection.
    pub to_upstream: u64,
    /// Bytes read from the destination, including those read by detection.
    pub to_client: u64,
    pub outcome: Outcome,
    pub error: Option<io::Error>,
}

/// A socket that counts what it reads and records, in `first_failure`, that it failed first.
struct Marked<'a> {
    inner: &'a mut TcpStream,
    side: u8,
    first_failure: &'a AtomicU8,
    read: u64,
}

impl Marked<'_> {
    fn note<T>(&self, result: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if matches!(result, Poll::Ready(Err(_))) {
            let _ = self.first_failure.compare_exchange(
                NONE,
                self.side,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
        result
    }
}

impl AsyncRead for Marked<'_> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut *this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.read += (buf.filled().len() - before) as u64;
        }
        this.note(result)
    }
}

impl AsyncWrite for Marked<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_write(cx, data);
        this.note(result)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_flush(cx);
        this.note(result)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_shutdown(cx);
        this.note(result)
    }
}

/// Writes the bytes read during detection to their side, then relays until both sides are done.
pub async fn tunnel(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    to_upstream: Vec<u8>,
    to_client: Vec<u8>,
) -> Tunnelled {
    let first_failure = AtomicU8::new(NONE);
    let mut client = Marked {
        inner: client,
        side: CLIENT,
        first_failure: &first_failure,
        read: to_upstream.len() as u64,
    };
    let mut upstream = Marked {
        inner: upstream,
        side: UPSTREAM,
        first_failure: &first_failure,
        read: to_client.len() as u64,
    };
    let (up, down) = tokio::join!(
        upstream.write_all(&to_upstream),
        client.write_all(&to_client)
    );
    let result = match (up, down) {
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ => copy_bidirectional(&mut client, &mut upstream)
            .await
            .map(|_| ()),
    };
    let (outcome, error) = match result {
        Ok(()) => (Outcome::Ok, None),
        Err(e) => match first_failure.load(Ordering::SeqCst) {
            UPSTREAM => (Outcome::UpstreamReset, Some(e)),
            _ => (Outcome::ClientAbort, Some(e)),
        },
    };
    Tunnelled {
        to_upstream: client.read,
        to_client: upstream.read,
        outcome,
        error,
    }
}
