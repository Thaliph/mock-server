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

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mimic_core::proxy::http1::{Codec, Event, HEAD_LIMIT, READ_AHEAD};
use prometheus_client::metrics::histogram::Histogram;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

use crate::access::Outcome;
use crate::load::{MARKER_HEADER, REQUEST_HEADER, Refusal, Watch};
use crate::mock::Mock;
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
    /// The mode switched to `mock` between two requests: the client bytes read and not
    /// forwarded, and when the first of them arrived. The destination connection is closed;
    /// the client side is left as it is, for the mock to continue.
    pub handover: Option<(Vec<u8>, Instant)>,
    /// A request carried a load marker that was refused (spec §4.1): nothing of it was
    /// forwarded and the earlier answers are written; the caller answers it and closes.
    pub refused: Option<Refusal>,
}

type Failure = (Side, io::Error);

struct State<F> {
    codec: Codec,
    on_event: F,
    overhead: Option<Overhead>,
    /// Events of bytes that were fed to the codec and are not yet written to their side: they
    /// are observed once the write is done (`with_deferred`, `flush`), in order, before any
    /// later event.
    deferred: Vec<Event>,
    /// The time the codec took on the bytes behind `deferred`.
    carried: Duration,
}

/// `mimic_proxy_overhead_seconds` of the connection: the time the sidecar spent on the request
/// head of each exchange (the codec and the observation of its events), kept until the
/// response head comes, and then observed with the time spent on that one. Waits (for the
/// upstream, for the pacing of the pipeline) and socket writes are not in it.
struct Overhead {
    histogram: Histogram,
    requests: VecDeque<Duration>,
}

impl Overhead {
    fn exchanges(&mut self, spent: Duration, requests: usize, responses: usize) {
        for _ in 0..requests {
            self.requests.push_back(spent);
        }
        for _ in 0..responses {
            let request = self.requests.pop_front().unwrap_or_default();
            self.histogram.observe((request + spent).as_secs_f64());
        }
    }
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
    /// Outbound discovery connections: the mock that takes the connection when the mode
    /// switches to `mock` (spec §8 "Switching").
    mock: Option<Mock>,
    /// The client pump stopped before the first byte of a request read after the switch.
    handover: Mutex<Option<(Vec<u8>, Instant)>>,
    handed: Notify,
    /// The connection became a tunnel (or unframed) after the switch: mock does not keep it.
    mock_close: Notify,
    /// Inbound connections with load testing on: every request head is read whole before any
    /// byte of it is forwarded, and the marker is judged and removed there (spec §4.1).
    watch: Option<Watch>,
    /// Per request read, whether it carried an admitted marker: its final response is marked.
    answers: Mutex<VecDeque<bool>>,
    /// The marker of a request was refused.
    refused: Mutex<Option<Refusal>>,
}

/// What the bytes read so far of a request head say about the load marker.
#[derive(Debug, PartialEq, Eq)]
enum Marked {
    /// The head is not complete.
    Open,
    /// The head is complete (its length) and carries `mimic-load-run`.
    Yes(usize),
    /// The head does not, or is not one the codec frames, or is too large to tell.
    No,
}

fn is_token(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Looks for the marker header in a request head, as the codec reads it (leading blank lines
/// skipped, `CRLF` or `LF` line ends). A head with a line the codec rejects is `No`: it is
/// relayed unchanged.
fn marked(held: &[u8]) -> Marked {
    let mut started = false;
    let mut found = false;
    let mut at = 0;
    while let Some(end) = held[at..].iter().position(|&b| b == b'\n') {
        let next = at + end + 1;
        let line = &held[at..next - 1];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        at = next;
        if line.is_empty() {
            if started {
                return if found { Marked::Yes(at) } else { Marked::No };
            }
            continue;
        }
        if !started {
            started = true;
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            return Marked::No;
        };
        if !is_token(&line[..colon]) {
            return Marked::No;
        }
        found |= line[..colon].eq_ignore_ascii_case(REQUEST_HEADER.as_bytes());
    }
    if held.len() >= HEAD_LIMIT {
        Marked::No
    } else {
        Marked::Open
    }
}

/// The value of every line of `head` named `name`, outer whitespace removed.
fn values<'a>(head: &'a [u8], name: &str) -> Vec<&'a [u8]> {
    head.split(|&b| b == b'\n')
        .skip(1)
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|&b| b == b':')?;
            line[..colon]
                .eq_ignore_ascii_case(name.as_bytes())
                .then(|| line[colon + 1..].trim_ascii())
        })
        .collect()
}

/// `head` without its lines named `name` (and the folded lines that follow them), and with
/// `add` (a complete line) before the blank line that ends it. `None` for a head with a folded
/// line: it is left as it is.
fn edited(head: &[u8], name: &str, add: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(head.len() + add.map_or(0, <[u8]>::len));
    let lines: Vec<&[u8]> = head.split_inclusive(|&b| b == b'\n').collect();
    let (last, lines) = lines.split_last()?;
    for (index, line) in lines.iter().enumerate() {
        if index > 0 && matches!(line.first(), Some(b' ' | b'\t')) {
            return None;
        }
        let colon = line.iter().position(|&b| b == b':');
        let named = index > 0
            && colon.is_some_and(|colon| line[..colon].eq_ignore_ascii_case(name.as_bytes()));
        if !named {
            out.extend_from_slice(line);
        }
    }
    if let Some(add) = add {
        out.extend_from_slice(add);
    }
    out.extend_from_slice(last);
    Some(out)
}

/// The end of the response head at the start of `held`, and its status.
fn response_head(held: &[u8]) -> Option<(usize, u16)> {
    let mut at = 0;
    let mut status = None;
    while let Some(end) = held[at..].iter().position(|&b| b == b'\n') {
        let line = &held[at..at + end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        at += end + 1;
        if line.is_empty() {
            return status.map(|status| (at, status));
        }
        if status.is_none() {
            status = std::str::from_utf8(line)
                .ok()?
                .split(' ')
                .nth(1)
                .and_then(|code| code.parse().ok());
            status?;
        }
    }
    None
}

impl<F: FnMut(Event)> Relay<F> {
    fn with<T>(&self, f: impl FnOnce(&mut Codec) -> (T, Vec<Event>)) -> T {
        self.run(f, false)
    }

    /// Like [`Relay::with`], but the events are only kept: the caller writes the bytes first
    /// and then calls [`Relay::flush`], so that the observation of an exchange (access line,
    /// metrics, discovery) never delays the bytes it describes (spec §4.1, PRX-33).
    fn with_deferred<T>(&self, f: impl FnOnce(&mut Codec) -> (T, Vec<Event>)) -> T {
        self.run(f, true)
    }

    /// Observes the deferred events.
    fn flush(&self) {
        self.run(|_| ((), Vec::new()), false);
    }

    fn run<T>(&self, f: impl FnOnce(&mut Codec) -> (T, Vec<Event>), defer: bool) -> T {
        let began = Instant::now();
        // A poisoned lock means the sink panicked: the relay task is gone anyway.
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = &mut *guard;
        let (out, events) = f(&mut state.codec);
        state.deferred.extend(events);
        if defer {
            state.carried += began.elapsed();
            return out;
        }
        let (mut requests, mut responses) = (0, 0);
        for event in state.deferred.drain(..) {
            match event {
                Event::Request(_) => requests += 1,
                Event::Response(_) => responses += 1,
                _ => {}
            }
            (state.on_event)(event);
        }
        let carried = std::mem::take(&mut state.carried);
        if requests + responses > 0
            && let Some(overhead) = state.overhead.as_mut()
        {
            overhead.exchanges(carried + began.elapsed(), requests, responses);
        }
        let opaque = state.codec.is_opaque();
        drop(guard);
        if opaque {
            self.close_if_mock();
        }
        out
    }

    /// An opaque connection (a tunnel, or unframed) is closed once the mode is `mock`.
    fn close_if_mock(&self) {
        if self.mock.as_ref().is_some_and(Mock::active) {
            self.mock_close.notify_one();
        }
    }

    /// Whether the mode is `mock` and the next client byte starts a request.
    fn switching(&self) -> bool {
        self.mock.as_ref().is_some_and(Mock::active)
            && self.with(|codec| (codec.between_requests(), Vec::new()))
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
        let mut read_at = Instant::now();
        // The client ended in the middle of a head: what was held goes on its way.
        let mut ended = false;
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
                read_at = Instant::now();
                held.extend_from_slice(&buf[..n]);
            }
            // The mode is read when a request's first byte is read: after a switch to mock
            // the request is not forwarded. The earlier ones are answered first.
            if self.switching() {
                while self.with(|codec| (codec.pending() > 0, Vec::new()))
                    && !self.server_closed.load(Ordering::Acquire)
                {
                    self.unblocked.notified().await;
                }
                self.to_upstream
                    .fetch_sub(held.len() as u64, Ordering::Relaxed);
                *self
                    .handover
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((held, read_at));
                self.handed.notify_one();
                return Ok(());
            }
            if let Some(watch) = self.watch.as_ref()
                && !ended
                && self.with(|codec| (codec.between_requests(), Vec::new()))
            {
                // A head waits for room first, so that it is judged once.
                if self.with(|codec| (codec.pending() >= READ_AHEAD, Vec::new()))
                    && !self.server_closed.load(Ordering::Acquire)
                {
                    self.unblocked.notified().await;
                    continue;
                }
                let verdict = match marked(&held) {
                    Marked::Open => {
                        // Held, not forwarded: the connection is not between two messages.
                        self.primed.store(false, Ordering::Release);
                        match tokio::time::timeout(watch.stall, client.read(&mut buf)).await {
                            // Nothing came for the stall: what is held goes on its way.
                            Err(_) => Marked::No,
                            Ok(read) => {
                                let n = read.map_err(|e| (Side::Client, e))?;
                                if n == 0 {
                                    ended = true;
                                } else {
                                    self.to_upstream.fetch_add(n as u64, Ordering::Relaxed);
                                    held.extend_from_slice(&buf[..n]);
                                }
                                continue;
                            }
                        }
                    }
                    verdict => verdict,
                };
                match verdict {
                    Marked::Yes(end) => {
                        match watch
                            .marker
                            .admit_values(&values(&held[..end], REQUEST_HEADER))
                        {
                            Err(refusal) => {
                                // The earlier requests are answered first, as for a switch.
                                while self.with(|codec| (codec.pending() > 0, Vec::new()))
                                    && !self.server_closed.load(Ordering::Acquire)
                                {
                                    self.unblocked.notified().await;
                                }
                                *self
                                    .refused
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(refusal);
                                self.handed.notify_one();
                                return Ok(());
                            }
                            Ok(run) => {
                                if let Some(tap) = &watch.tap {
                                    tap.load_exchange();
                                }
                                if let Some(head) = edited(&held[..end], REQUEST_HEADER, None) {
                                    self.to_upstream
                                        .fetch_sub((end - head.len()) as u64, Ordering::Relaxed);
                                    held.splice(..end, head);
                                }
                                self.answered(true);
                                watch.pushed(Some(run));
                            }
                        }
                    }
                    _ => {
                        self.answered(false);
                        watch.pushed(None);
                    }
                }
            }
            let used = self.with_deferred(|codec| codec.client_bytes_limited(&held));
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
            self.flush();
            held.drain(..used);
        }
    }

    /// Destination to client.
    async fn server_pump(
        &self,
        upstream: &mut (impl AsyncRead + Unpin),
        client: &mut (impl AsyncWrite + Unpin),
    ) -> Result<(), Failure> {
        if let Some(watch) = &self.watch {
            return self.server_pump_marked(watch, upstream, client).await;
        }
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
            let answered = self.with_deferred(|codec| {
                let before = codec.pending();
                let events = codec.server_bytes(&buf[..n]);
                (codec.pending() < before || !events.is_empty(), events)
            });
            send(client, &buf[..n])
                .await
                .map_err(|e| (Side::Client, e))?;
            self.flush();
            if answered {
                self.unblocked.notify_one();
            }
            self.close_if_idle();
        }
    }

    /// A request head was passed on: whether its final response is a load request's.
    fn answered(&self, load: bool) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(load);
    }

    /// Destination to client when load testing is on: the head of the final response to a load
    /// request is held whole to lose its `x-mimic-load` lines and gain one `ok`; every other
    /// byte goes through as in [`Relay::server_pump`].
    async fn server_pump_marked(
        &self,
        watch: &Watch,
        upstream: &mut (impl AsyncRead + Unpin),
        client: &mut (impl AsyncWrite + Unpin),
    ) -> Result<(), Failure> {
        let mut buf = vec![0u8; CHUNK];
        let mut held: Vec<u8> = Vec::new();
        let mut release = false;
        loop {
            if held.is_empty() {
                release = false;
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
                held.extend_from_slice(&buf[..n]);
            }
            let load = self
                .answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .front()
                .copied()
                .unwrap_or(false);
            if load && !release && self.with(|codec| (codec.between_responses(), Vec::new())) {
                match response_head(&held) {
                    None if held.len() < HEAD_LIMIT => {
                        match tokio::time::timeout(watch.stall, upstream.read(&mut buf)).await {
                            Ok(Ok(0)) | Err(_) => release = true,
                            Ok(Ok(n)) => {
                                self.to_client.fetch_add(n as u64, Ordering::Relaxed);
                                held.extend_from_slice(&buf[..n]);
                            }
                            Ok(Err(e)) => return Err((Side::Upstream, e)),
                        }
                        continue;
                    }
                    // An interim response is not the answer.
                    Some((end, status)) if status >= 200 || status == 101 => {
                        let ok = format!("{MARKER_HEADER}: ok\r\n");
                        if let Some(head) = edited(&held[..end], MARKER_HEADER, Some(ok.as_bytes()))
                        {
                            held.splice(..end, head);
                        }
                    }
                    _ => {}
                }
            }
            let ((used, popped, seen), ()) = {
                let out = self.with(|codec| {
                    let before = codec.pending();
                    let (used, events) = codec.server_bytes_limited(&held);
                    (
                        (
                            used,
                            before.saturating_sub(codec.pending()),
                            !events.is_empty(),
                        ),
                        events,
                    )
                });
                (out, ())
            };
            {
                let mut answers = self
                    .answers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for _ in 0..popped {
                    answers.pop_front();
                }
            }
            send(client, &held[..used])
                .await
                .map_err(|e| (Side::Client, e))?;
            held.drain(..used);
            if popped > 0 || seen {
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

/// Closes the destination side with a FIN once every answer was read, then reads and drops
/// what it still sends until it closes (or [`IDLE_DRAIN`] passes): no reset.
async fn close_upstream(upstream: &mut Stream) {
    let _ = tokio::time::timeout(IDLE_DRAIN, async {
        let _ = upstream.shutdown().await;
        let mut buf = vec![0u8; CHUNK];
        while matches!(upstream.read(&mut buf).await, Ok(n) if n > 0) {}
    })
    .await;
}

/// Answers a refused load request (spec §4.1) and closes: the answer, a FIN, then what the
/// client still sends is read and dropped (at most [`IDLE_DRAIN`]), so that it is no reset.
pub async fn refuse(client: &mut Stream, refusal: Refusal) {
    let answer = format!(
        "HTTP/1.1 599 Refused\r\n{MARKER_HEADER}: refused; reason={}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        refusal.as_str()
    );
    if send(client, answer.as_bytes()).await.is_ok() {
        close_upstream(client).await;
    }
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
    relay_with(
        client,
        upstream,
        first,
        false,
        None,
        None,
        None,
        on_event,
        stop,
        abort_signal,
    )
    .await
}

/// [`relay`], with a codec that also reports [`Event::Captured`] when `capture` is set (the
/// discovery tap, spec §6). With a `mock`, the relay ends when the mode switches to `mock`:
/// before the first byte of the next request (see [`Relayed::handover`]), or at once for a
/// connection that became a tunnel (outcome `MockClosed`). With a `watch` (inbound, load
/// testing on) each request head is held whole and judged for `mimic-load-run`, rewritten in
/// place when admitted, and the relay ends before any byte of a refused one
/// ([`Relayed::refused`]).
#[allow(clippy::too_many_arguments)]
pub async fn relay_with(
    client: &mut Stream,
    upstream: &mut Stream,
    first: Vec<u8>,
    capture: bool,
    mock: Option<Mock>,
    watch: Option<Watch>,
    overhead: Option<Histogram>,
    on_event: impl FnMut(Event) + Send,
    stop: impl Future<Output = ()> + Send,
    abort_signal: impl Future<Output = ()> + Send,
) -> Relayed {
    let (client_socket, upstream_socket) = (client.socket(), upstream.socket());
    let relay = Relay {
        state: Mutex::new(State {
            codec: {
                let codec = if capture {
                    Codec::with_capture()
                } else {
                    Codec::new()
                };
                if watch.is_some() {
                    codec.one_request_at_a_time()
                } else {
                    codec
                }
            },
            on_event,
            overhead: overhead.map(|histogram| Overhead {
                histogram,
                requests: VecDeque::new(),
            }),
            deferred: Vec::new(),
            carried: Duration::ZERO,
        }),
        unblocked: Notify::new(),
        stopping: AtomicBool::new(false),
        closing: Notify::new(),
        primed: AtomicBool::new(false),
        server_closed: AtomicBool::new(false),
        closed_first: AtomicU8::new(NONE),
        to_upstream: AtomicU64::new(0),
        to_client: AtomicU64::new(0),
        mock,
        handover: Mutex::new(None),
        handed: Notify::new(),
        mock_close: Notify::new(),
        watch,
        answers: Mutex::new(VecDeque::new()),
        refused: Mutex::new(None),
    };
    let mut mock_closed = false;
    let mut handed = false;
    let ended = {
        let mut abort_signal = pin!(abort_signal);
        let mut stop = pin!(stop);
        let mut stopping = false;
        let mut idle_close = false;
        let mut entered = pin!(async {
            match &relay.mock {
                Some(mock) => mock.entered().await,
                None => std::future::pending().await,
            }
        });
        let mut entered_seen = false;
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
                () = &mut entered, if !entered_seen => {
                    entered_seen = true;
                    // A tunnel is not kept by the mock (a request is handed over when it is read).
                    if relay.with(|codec| (codec.is_opaque(), Vec::new())) {
                        relay.close_if_mock();
                    }
                }
                () = relay.mock_close.notified() => {
                    mock_closed = true;
                    break Ok(());
                }
                () = relay.handed.notified() => {
                    handed = true;
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
    if handed {
        close_upstream(upstream).await;
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
        Ok(()) if mock_closed => (Outcome::MockClosed, None),
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
        handover: relay
            .handover
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(),
        refused: relay
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(),
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

    /// PRX-33: the observation of a response (access line, metrics, discovery) comes after its
    /// bytes were written to the client, so a slow sink cannot delay them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prx_33_response_bytes_do_not_wait_for_their_observation() {
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        let ((mut client, mut relay_client), (mut dest, mut relay_dest)) =
            (pair().await, pair().await);
        let observed = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&observed);
        let relayed = tokio::spawn(async move {
            relay(
                &mut relay_client,
                &mut relay_dest,
                Vec::new(),
                move |event| {
                    if matches!(event, Event::Response(_)) {
                        // A sink that takes 500 ms, as a recorder under load could.
                        std::thread::sleep(Duration::from_millis(500));
                        seen.store(true, Ordering::SeqCst);
                    }
                },
                std::future::pending(),
                std::future::pending(),
            )
            .await
            .outcome
        });
        client.write_all(REQUEST).await.unwrap();
        let mut got = vec![0u8; REQUEST.len()];
        dest.read_exact(&mut got).await.unwrap();
        dest.write_all(RESPONSE).await.unwrap();
        let mut answer = vec![0u8; RESPONSE.len()];
        timeout(Duration::from_millis(250), client.read_exact(&mut answer))
            .await
            .expect("the response waited for its observation")
            .unwrap();
        assert_eq!(answer, RESPONSE);
        assert!(!observed.load(Ordering::SeqCst), "the sink was not slow");
        drop((client, dest));
        let _ = relayed.await;
    }
}
