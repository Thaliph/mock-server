//! The HTTP/1 byte relay (spec §4.1): every byte read from one side is written to the other
//! unchanged and in order, as soon as it is read. The [`Codec`] only finds message boundaries;
//! it paces the client direction (at most `READ_AHEAD` requests ahead of their responses, no
//! client bytes behind an upgrade request until its response) and reports [`Event`]s. After
//! `Tunnel` or `Unframed` the codec is silent and the two pumps are a plain opaque relay.
//! A FIN reaches the other side as a FIN once every byte read before it was written; a reset
//! (or any socket failure) reaches it as a reset, also after a FIN. On `stop` (shutdown) the
//! connection is closed, nothing added, as soon as the codec is between two messages.

use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use mimic_core::proxy::http1::{Codec, Event};
use socket2::SockRef;
use tokio::net::TcpStream;
use tokio::sync::Notify;

use crate::access::Outcome;
use crate::opaque::{Side, abort, failed, write_all};

const CHUNK: usize = 16 * 1024;
/// Values of [`Relay::closed_first`].
const NONE: u8 = 0;
const CLIENT: u8 = 1;
const SERVER: u8 = 2;

/// How a relay ended.
pub struct Relayed {
    pub outcome: Outcome,
    /// The socket failure that ended the relay, if any.
    pub error: Option<io::Error>,
    /// Bytes read from the client, including `first`.
    pub to_upstream: u64,
    /// Bytes read from the destination.
    pub to_client: u64,
    /// Requests read and never answered by a final response.
    pub unanswered: usize,
    /// Payload bytes of the request body left incomplete, and of the response body.
    pub partial_body: (u64, u64),
}

type Failure = (Side, io::Error);

struct State<F> {
    codec: Codec,
    on_event: F,
}

struct Relay<F> {
    state: Mutex<State<F>>,
    /// A response completed, or the codec went silent: the client pump may use more bytes.
    unblocked: Notify,
    /// The shutdown started: close at the next idle point.
    stopping: AtomicBool,
    /// The connection is idle while stopping: the relay can end.
    closing: Notify,
    /// The destination sent its FIN: a gated client pump has nowhere to forward to.
    server_closed: AtomicBool,
    /// Which side sent its FIN first: `NONE`, `CLIENT` or `SERVER`.
    closed_first: AtomicU8,
    to_upstream: AtomicU64,
    to_client: AtomicU64,
}

impl<F: FnMut(Event)> Relay<F> {
    fn with<T>(&self, f: impl FnOnce(&mut Codec) -> (T, Vec<Event>)) -> T {
        // A poisoned lock means the sink panicked: the relay task is gone anyway.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (out, events) = f(&mut state.codec);
        for event in events {
            (state.on_event)(event);
        }
        out
    }

    /// While stopping, ends the relay once nothing is pending and nothing is half-read.
    fn close_if_idle(&self) {
        if self.stopping.load(Ordering::Relaxed) && self.with(|codec| (codec.idle(), Vec::new())) {
            self.closing.notify_one();
        }
    }

    fn first_to_close(&self, side: u8) {
        let _ =
            self.closed_first
                .compare_exchange(NONE, side, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Client to destination.
    async fn client_pump(
        &self,
        client: &TcpStream,
        upstream: &TcpStream,
        first: Vec<u8>,
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; CHUNK];
        let mut held = first;
        self.to_upstream
            .fetch_add(held.len() as u64, Ordering::Relaxed);
        loop {
            if held.is_empty() {
                let n = read_some(client, &mut buf)
                    .await
                    .map_err(|e| (Side::Client, e))?;
                if n == 0 {
                    self.first_to_close(CLIENT);
                    return fin(upstream, Side::Upstream);
                }
                self.to_upstream.fetch_add(n as u64, Ordering::Relaxed);
                held.extend_from_slice(&buf[..n]);
            }
            let used = self.with(|codec| codec.client_bytes_limited(&held));
            if used == 0 {
                // The destination is gone: what is held has nowhere to go (an upgrade or
                // read-ahead gate would never open). The relay ends, both sockets close.
                if self.server_closed.load(Ordering::Acquire) {
                    return Ok(());
                }
                // Back-pressure: the unused bytes stay here and the socket stays unread. A
                // reset of the client is still seen by the error watch in `relay`.
                self.unblocked.notified().await;
                continue;
            }
            write_all(upstream, &held[..used])
                .await
                .map_err(|e| (Side::Upstream, e))?;
            held.drain(..used);
        }
    }

    /// Destination to client.
    async fn server_pump(&self, upstream: &TcpStream, client: &TcpStream) -> Result<(), Failure> {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = read_some(upstream, &mut buf)
                .await
                .map_err(|e| (Side::Upstream, e))?;
            if n == 0 {
                self.first_to_close(SERVER);
                self.with(|codec| ((), codec.server_eof()));
                self.server_closed.store(true, Ordering::Release);
                self.unblocked.notify_one();
                return fin(client, Side::Client);
            }
            self.to_client.fetch_add(n as u64, Ordering::Relaxed);
            let answered = self.with(|codec| {
                let before = codec.pending();
                let events = codec.server_bytes(&buf[..n]);
                (codec.pending() < before || !events.is_empty(), events)
            });
            write_all(client, &buf[..n])
                .await
                .map_err(|e| (Side::Client, e))?;
            if answered {
                self.unblocked.notify_one();
            }
            self.close_if_idle();
        }
    }
}

async fn read_some(from: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        from.readable().await?;
        match from.try_read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
}

/// Passes a FIN on.
fn fin(to: &TcpStream, side: Side) -> Result<(), Failure> {
    SockRef::from(to)
        .shutdown(Shutdown::Write)
        .or_else(|e| match e.kind() {
            io::ErrorKind::NotConnected => Ok(()),
            _ => Err((side, e)),
        })
}

/// Relays one HTTP/1 connection until both sides are done, a side fails (the other one gets a
/// reset), `stop` resolves and the connection is idle (no byte added: both sides see a FIN),
/// or `abort_signal` resolves. `first` are the client bytes already read (by protocol
/// detection); `on_event` receives the codec's events in order.
pub async fn relay(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    first: Vec<u8>,
    on_event: impl FnMut(Event) + Send,
    stop: impl Future<Output = ()> + Send,
    abort_signal: impl Future<Output = ()> + Send,
) -> Relayed {
    let (client, upstream) = (&*client, &*upstream);
    let relay = Relay {
        state: Mutex::new(State {
            codec: Codec::new(),
            on_event,
        }),
        unblocked: Notify::new(),
        stopping: AtomicBool::new(false),
        closing: Notify::new(),
        server_closed: AtomicBool::new(false),
        closed_first: AtomicU8::new(NONE),
        to_upstream: AtomicU64::new(0),
        to_client: AtomicU64::new(0),
    };
    let ended = {
        let mut abort_signal = pin!(abort_signal);
        let mut stop = pin!(stop);
        let mut stopping = false;
        let mut pumps = pin!(async {
            tokio::try_join!(
                relay.client_pump(client, upstream, first),
                relay.server_pump(upstream, client),
            )
        });
        loop {
            tokio::select! {
                r = &mut pumps => break r.map(|_| ()).map_err(Some),
                (side, e) = failed(client, Side::Client) => break Err(Some((side, e))),
                (side, e) = failed(upstream, Side::Upstream) => break Err(Some((side, e))),
                () = &mut abort_signal => break Err(None),
                () = &mut stop, if !stopping => {
                    stopping = true;
                    relay.stopping.store(true, Ordering::Relaxed);
                    relay.close_if_idle();
                }
                () = relay.closing.notified() => break Ok(()),
            }
        }
    };
    let unanswered = relay.with(|codec| (codec.pending(), Vec::new()));
    let partial_body = relay.with(|codec| {
        (
            (codec.request_body_bytes(), codec.response_body_bytes()),
            Vec::new(),
        )
    });
    let (outcome, error) = match ended {
        Ok(()) if unanswered > 0 && relay.closed_first.load(Ordering::Relaxed) == CLIENT => {
            (Outcome::ClientAbort, None)
        }
        Ok(()) if unanswered > 0 => (Outcome::UpstreamReset, None),
        Ok(()) => (Outcome::Ok, None),
        Err(None) => (Outcome::Shutdown, None),
        Err(Some((Side::Upstream, e))) => {
            // The client sees what the destination did: a reset.
            abort(client);
            (Outcome::UpstreamReset, Some(e))
        }
        Err(Some((Side::Client, e))) => {
            abort(upstream);
            (Outcome::ClientAbort, Some(e))
        }
    };
    Relayed {
        outcome,
        error,
        to_upstream: relay.to_upstream.load(Ordering::Relaxed),
        to_client: relay.to_client.load(Ordering::Relaxed),
        unanswered,
        partial_body,
    }
}
