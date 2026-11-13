//! The head reader between the app's socket and `hyper` (spec §4.1 "Mesh-routed HTTP", limits
//! 1 and 8): [`Gate`] is the IO `hyper` reads, and what it lets through is what
//! [`mimic_core::mesh::head::Reader`] passes. At a refused head or an `Upgrade` it reads end of
//! file, so that `hyper` answers what it holds and finishes; the sidecar then answers the
//! refusal itself or takes the socket over.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use mimic_core::mesh::head::{Reader, Stop};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::transport::Stream;

const READ_SIZE: usize = 16 * 1024;

/// What the reader decided, read by whoever serves the connection once `hyper` is done.
pub(crate) type Verdict = Arc<Mutex<Option<(Stop, Vec<u8>)>>>;

/// The requests `hyper` was given and the ones it finished answering: the reader signals end of
/// file only when they are equal, because `hyper` abandons a request in flight when it reads one.
#[derive(Default)]
pub(crate) struct Progress {
    passed: AtomicUsize,
    finished: AtomicUsize,
    waker: Mutex<Option<Waker>>,
}

impl Progress {
    /// A request was answered (its response body was dropped).
    pub(crate) fn finish(&self) {
        self.finished.fetch_add(1, Ordering::AcqRel);
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether every request given to `hyper` was answered; otherwise `waker` is woken when one is.
    fn idle(&self, waker: &Waker) -> bool {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker.clone());
        self.finished.load(Ordering::Acquire) >= self.passed.load(Ordering::Acquire)
    }
}

pub(crate) struct Gate {
    stream: Stream,
    reader: Reader,
    /// Whether an `Upgrade` or `CONNECT` head leaves `hyper` now (not while the mock answers).
    upgrades: Box<dyn Fn() -> bool + Send>,
    /// Bytes `hyper` may take, and how many of them it took.
    ready: Vec<u8>,
    taken: usize,
    scratch: Box<[u8; READ_SIZE]>,
    verdict: Verdict,
    progress: Arc<Progress>,
    stopped: bool,
}

impl Gate {
    pub(crate) fn new(
        stream: Stream,
        upgrades: impl Fn() -> bool + Send + 'static,
        progress: Arc<Progress>,
    ) -> (Gate, Verdict) {
        let verdict = Verdict::default();
        let gate = Gate {
            stream,
            reader: Reader::new(),
            upgrades: Box::new(upgrades),
            ready: Vec::new(),
            taken: 0,
            scratch: Box::new([0; READ_SIZE]),
            verdict: verdict.clone(),
            progress,
            stopped: false,
        };
        (gate, verdict)
    }

    pub(crate) fn into_stream(self) -> Stream {
        self.stream
    }
}

impl AsyncRead for Gate {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.taken < this.ready.len() {
                let n = (this.ready.len() - this.taken).min(buf.remaining());
                buf.put_slice(&this.ready[this.taken..this.taken + n]);
                this.taken += n;
                if this.taken == this.ready.len() {
                    this.ready.clear();
                    this.taken = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.stopped {
                return if this.progress.idle(cx.waker()) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                };
            }
            let mut read = ReadBuf::new(&mut this.scratch[..]);
            match Pin::new(&mut this.stream).poll_read(cx, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
            let data = read.filled();
            if data.is_empty() {
                return Poll::Ready(Ok(()));
            }
            let upgrades = (this.upgrades)();
            let stop = this.reader.feed(data, upgrades, &mut this.ready);
            this.progress
                .passed
                .store(this.reader.heads(), Ordering::Release);
            if let Some(stop) = stop {
                let held = this.reader.held().to_vec();
                *this.verdict.lock().unwrap_or_else(PoisonError::into_inner) = Some((stop, held));
                this.stopped = true;
            }
        }
    }
}

impl AsyncWrite for Gate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    /// No FIN when the sidecar still has something to write or take over the socket.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.stopped {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
