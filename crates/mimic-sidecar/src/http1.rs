//! The HTTP/1 byte relay (spec §4.1): every byte read from one side is written to the other
//! unchanged and in order, as soon as it is read. The [`Codec`] only finds message boundaries;
//! it paces the client direction (at most `READ_AHEAD` requests ahead of their responses, no
//! client bytes behind an upgrade request until its response) and reports [`Event`]s. After
//! `Tunnel` or `Unframed` the codec is silent and the two pumps are a plain opaque relay.
//! A FIN reaches the other side as a FIN once every byte read before it was written; a reset
//! (or any socket failure) reaches it as a reset, also after a FIN. Each side is a plain TCP
//! or a TLS [`Stream`]: over TLS the bytes are the decrypted ones and a FIN is `close_notify`
//! then FIN. On `stop` (shutdown) the
//! connection is closed, nothing added, as soon as the codec is between two messages: a FIN on
//! both sides, after what the peers still sent was read and dropped (no reset).

use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use mimic_core::proxy::http1::{Codec, Event};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

use crate::access::Outcome;
use crate::opaque::{Side, abort, failed};
use crate::transport::Stream;

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
    /// `notify_one` stores a permit when the pump is not waiting yet, so a response that
    /// completes between the pump's refusal (`used == 0`) and its `notified().await` is not
    /// lost; the pump then retries once and, if still gated, waits again.
    unblocked: Notify,
    /// The shutdown started: close at the next idle point.
    stopping: AtomicBool,
    /// The connection is idle while stopping: the relay can end.
    closing: Notify,
    /// The client pump has written the bytes it was given (`first`): until then the codec has
    /// not seen them and looks idle.
    primed: AtomicBool,
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
        if self.stopping.load(Ordering::Relaxed)
            && self.primed.load(Ordering::Acquire)
            && self.with(|codec| (codec.idle(), Vec::new()))
        {
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
        client: &mut (impl AsyncRead + Unpin),
        upstream: &mut (impl AsyncWrite + Unpin),
        first: Vec<u8>,
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; CHUNK];
        let mut held = first;
        self.to_upstream
            .fetch_add(held.len() as u64, Ordering::Relaxed);
        loop {
            if held.is_empty() {
                // Everything handed over has been through the codec: it may now be idle.
                self.primed.store(true, Ordering::Release);
                self.close_if_idle();
                let n = client.read(&mut buf).await.map_err(|e| (Side::Client, e))?;
                if n == 0 {
                    self.first_to_close(CLIENT);
                    return fin(upstream, Side::Upstream).await;
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
            send(upstream, &held[..used])
                .await
                .map_err(|e| (Side::Upstream, e))?;
            held.drain(..used);
        }
    }

    /// Destination to client.
    async fn server_pump(
        &self,
        upstream: &mut (impl AsyncRead + Unpin),
        client: &mut (impl AsyncWrite + Unpin),
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = upstream
                .read(&mut buf)
                .await
                .map_err(|e| (Side::Upstream, e))?;
            if n == 0 {
                self.first_to_close(SERVER);
                self.with(|codec| ((), codec.server_eof()));
                self.server_closed.store(true, Ordering::Release);
                self.unblocked.notify_one();
                return fin(client, Side::Client).await;
            }
            self.to_client.fetch_add(n as u64, Ordering::Relaxed);
            let answered = self.with(|codec| {
                let before = codec.pending();
                let events = codec.server_bytes(&buf[..n]);
                (codec.pending() < before || !events.is_empty(), events)
            });
            send(client, &buf[..n])
                .await
                .map_err(|e| (Side::Client, e))?;
            if answered {
                self.unblocked.notify_one();
            }
            self.close_if_idle();
        }
    }
}

/// Writes `data` and pushes it out (a TLS stream buffers until flushed).
async fn send(to: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> io::Result<()> {
    to.write_all(data).await?;
    to.flush().await
}

/// Passes a FIN on.
async fn fin(to: &mut (impl AsyncWrite + Unpin), side: Side) -> Result<(), Failure> {
    to.shutdown().await.map_err(|e| (side, e))
}

/// Most time the idle close waits for the peers to read the FIN and close.
const IDLE_DRAIN: Duration = Duration::from_millis(250);

/// Sends a FIN on both streams, then reads and drops what the peers still send until they
/// close (or [`IDLE_DRAIN`] passes).
async fn close_idle(client: &mut Stream, upstream: &mut Stream) {
    async fn drain(stream: &mut Stream) {
        let _ = stream.shutdown().await;
        let mut buf = vec![0u8; CHUNK];
        while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
    }
    let _ = tokio::time::timeout(IDLE_DRAIN, async {
        tokio::join!(drain(client), drain(upstream));
    })
    .await;
}

/// Relays one HTTP/1 connection until both sides are done, a side fails (the other one gets a
/// reset), `stop` resolves and the connection is idle (no byte added: both sides see a FIN),
/// or `abort_signal` resolves. `first` are the client bytes already read (by protocol
/// detection); `on_event` receives the codec's events in order.
pub async fn relay(
    client: &mut Stream,
    upstream: &mut Stream,
    first: Vec<u8>,
    on_event: impl FnMut(Event) + Send,
    stop: impl Future<Output = ()> + Send,
    abort_signal: impl Future<Output = ()> + Send,
) -> Relayed {
    relay_with(client, upstream, first, false, on_event, stop, abort_signal).await
}

/// [`relay`], with a codec that also reports [`Event::Captured`] when `capture` is set (the
/// discovery tap, spec §6).
pub async fn relay_with(
    client: &mut Stream,
    upstream: &mut Stream,
    first: Vec<u8>,
    capture: bool,
    on_event: impl FnMut(Event) + Send,
    stop: impl Future<Output = ()> + Send,
    abort_signal: impl Future<Output = ()> + Send,
) -> Relayed {
    let (client_socket, upstream_socket) = (client.socket(), upstream.socket());
    let relay = Relay {
        state: Mutex::new(State {
            codec: if capture {
                Codec::with_capture()
            } else {
                Codec::new()
            },
            on_event,
        }),
        unblocked: Notify::new(),
        stopping: AtomicBool::new(false),
        closing: Notify::new(),
        primed: AtomicBool::new(false),
        server_closed: AtomicBool::new(false),
        closed_first: AtomicU8::new(NONE),
        to_upstream: AtomicU64::new(0),
        to_client: AtomicU64::new(0),
    };
    let ended = {
        let mut abort_signal = pin!(abort_signal);
        let mut stop = pin!(stop);
        let mut stopping = false;
        let mut idle_close = false;
        let (mut client_read, mut client_write) = tokio::io::split(&mut *client);
        let (mut upstream_read, mut upstream_write) = tokio::io::split(&mut *upstream);
        let mut pumps = pin!(async {
            tokio::try_join!(
                relay.client_pump(&mut client_read, &mut upstream_write, first),
                relay.server_pump(&mut upstream_read, &mut client_write),
            )
        });
        loop {
            tokio::select! {
                r = &mut pumps => break r.map(|_| ()).map_err(Some),
                (side, e) = failed(client_socket.tcp(), Side::Client) => break Err(Some((side, e))),
                (side, e) = failed(upstream_socket.tcp(), Side::Upstream) => break Err(Some((side, e))),
                () = &mut abort_signal => break Err(None),
                () = &mut stop, if !stopping => {
                    stopping = true;
                    relay.stopping.store(true, Ordering::Relaxed);
                    relay.close_if_idle();
                }
                () = relay.closing.notified() => {
                    idle_close = true;
                    break Ok(());
                }
            }
        }
        .map(|()| idle_close)
    };
    // Closing a socket that holds unread bytes sends a reset: the idle close is a FIN on both
    // sides, and what the client sent meanwhile is read and dropped before the caller closes.
    if matches!(ended, Ok(true)) {
        close_idle(client, upstream).await;
    }
    let ended = ended.map(|_| ());
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
        // The only place that names unanswered requests (`Http1Tracker::finish` takes it as is).
        Ok(()) if unanswered > 0 => (Outcome::UpstreamReset, None),
        Ok(()) => (Outcome::Ok, None),
        Err(None) => (Outcome::Shutdown, None),
        Err(Some((Side::Upstream, e))) => {
            // The client sees what the destination did: a reset.
            abort(client_socket.tcp());
            (Outcome::UpstreamReset, Some(e))
        }
        Err(Some((Side::Client, e))) => {
            abort(upstream_socket.tcp());
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    use super::*;

    /// A connected pair: (the test's end, the relay's end).
    async fn pair() -> (TcpStream, Stream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let near = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (far, _) = listener.accept().await.unwrap();
        (near, Stream::plain(far))
    }

    const REQUEST: &[u8] = b"GET /a HTTP/1.1\r\nHost: a\r\n\r\n";

    #[tokio::test]
    async fn prx_13_detection_bytes_survive_a_shutdown_already_started() {
        for round in 0..40 {
            let ((client, mut relay_client), (mut dest, mut relay_dest)) =
                (pair().await, pair().await);
            let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
            let relayed = tokio::spawn(async move {
                relay(
                    &mut relay_client,
                    &mut relay_dest,
                    REQUEST.to_vec(),
                    |_| {},
                    std::future::ready(()),
                    async {
                        let _ = abort_rx.await;
                    },
                )
                .await
                .outcome
            });
            let mut got = vec![0u8; REQUEST.len()];
            timeout(Duration::from_secs(2), dest.read_exact(&mut got))
                .await
                .unwrap_or_else(|_| panic!("round {round}: the detection bytes were dropped"))
                .unwrap();
            assert_eq!(got, REQUEST);
            drop(abort_tx);
            let _ = relayed.await;
            drop(client);
        }
    }

    /// Reads to the end: `Ok` when the peer closed with a FIN, the error when it reset.
    async fn end_of(s: &mut TcpStream) -> io::Result<()> {
        let mut buf = [0u8; 64];
        timeout(Duration::from_secs(3), async {
            loop {
                if s.read(&mut buf).await? == 0 {
                    return Ok(());
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(io::Error::other("the connection stayed open")))
    }

    #[tokio::test]
    async fn prx_13_idle_close_with_unread_client_bytes_is_a_fin() {
        for round in 0..20 {
            let ((mut client, mut relay_client), (mut dest, mut relay_dest)) =
                (pair().await, pair().await);
            // Bytes that reached the socket after the connection was found idle.
            client.write_all(b"GET /late HTT").await.unwrap();
            dest.write_all(b"late too").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            let closing = tokio::spawn(async move {
                close_idle(&mut relay_client, &mut relay_dest).await;
                drop((relay_client, relay_dest));
            });
            // The peers see the FIN (not a reset), then close.
            end_of(&mut client)
                .await
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
            end_of(&mut dest)
                .await
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
            drop((client, dest));
            closing.await.unwrap();
        }
    }

    #[tokio::test]
    async fn prx_13_idle_relay_closes_both_sides_with_a_fin() {
        let ((mut client, mut relay_client), (mut dest, mut relay_dest)) =
            (pair().await, pair().await);
        let stop = Arc::new(Notify::new());
        let stopper = Arc::clone(&stop);
        let relayed = tokio::spawn(async move {
            let outcome = relay(
                &mut relay_client,
                &mut relay_dest,
                Vec::new(),
                |_| {},
                async move { stopper.notified().await },
                std::future::pending(),
            )
            .await
            .outcome;
            drop((relay_client, relay_dest));
            outcome
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.notify_one();
        end_of(&mut client).await.expect("a FIN to the client");
        end_of(&mut dest).await.expect("a FIN to the destination");
        assert_eq!(relayed.await.unwrap(), Outcome::Ok);
    }
}
