//! Mock mode of the sidecar (spec §8): outbound HTTP/1 answered from the egress snapshot,
//! with the drawn latency, the pending cap, the miss log and the `mimic_mock_*` metrics. The
//! matching, the answer and the draw are pure and live in `mimic_core::mock`; this module
//! owns the sockets, the timers and the random numbers.

use std::collections::BTreeSet;
use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::service::Service as HyperService;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use mimic_core::Mode;
use mimic_core::discovery::redact::{self, Redactor};
use mimic_core::mock::{self, Miss, Random};
use mimic_core::proxy::authority::{normalise_authority, target_authority};
use mimic_core::proxy::http1::CAPTURE_BODY_LIMIT;
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::access::{self, Outcome, truncate_path};
use crate::discovery::CaptureBudget;
use crate::discovery::decode::decode;
use crate::egress::Snapshot;
use crate::exchange::Ctx;
use crate::transport::Stream;

type BoxError = Box<dyn Error + Send + Sync>;

const PROTOCOL_H1: &str = "http1";
/// Signatures written to the log per mode period (spec §8 "Miss").
const MISS_LOG_LIMIT: usize = 256;
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;
/// Largest mocked body of a request kept for matching: the capture limit of spec §6.
/// The capture budget when the mock has none of discovery's.
const DEFAULT_BUDGET: usize = 8 * 1024 * 1024;
const MAX_MATCHED_BODY: usize = CAPTURE_BODY_LIMIT;

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ReasonLabel {
    reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DecisionLabel {
    decision: &'static str,
}

/// The pending count and the overload state, under one lock.
#[derive(Default)]
struct Slots {
    pending: usize,
    overloaded: bool,
}

/// What the miss log has written in the current mode period.
#[derive(Default)]
struct MissLog {
    epoch: u64,
    seen: BTreeSet<(&'static str, String, String, String)>,
    limit_written: bool,
}

struct Inner {
    snapshot: Snapshot,
    redactor: Redactor,
    budget: CaptureBudget,
    max_pending: usize,
    slots: Mutex<Slots>,
    pending: Gauge,
    requests: Family<ResultLabel, Counter>,
    misses: Family<ReasonLabel, Counter>,
    connections: Family<DecisionLabel, Counter>,
    miss_log: Mutex<MissLog>,
    /// Milliseconds a mocked connection may stay silent in the middle of a request.
    stall_ms: AtomicU64,
}

/// The mock of a sidecar: answers outbound requests from the snapshot while the mode is
/// `mock`. Cheap to clone.
#[derive(Clone)]
pub struct Mock(Arc<Inner>);

impl Mock {
    /// `max_pending` is `MIMIC_MOCK_MAX_PENDING`. The request bodies being matched are kept
    /// in a capture budget of their own, see [`Mock::with_budget`].
    pub fn new(snapshot: Snapshot, max_pending: usize, redactor: Redactor) -> Mock {
        let budget = CaptureBudget::new(DEFAULT_BUDGET);
        Mock::with_budget(snapshot, max_pending, redactor, budget)
    }

    /// Like [`Mock::new`], keeping the bodies being matched in `budget`
    /// (`MIMIC_DISCOVERY_CAPTURE_BUDGET`), shared with discovery.
    pub fn with_budget(
        snapshot: Snapshot,
        max_pending: usize,
        redactor: Redactor,
        budget: CaptureBudget,
    ) -> Mock {
        Mock(Arc::new(Inner {
            snapshot,
            redactor,
            budget,
            max_pending,
            slots: Mutex::default(),
            pending: Gauge::default(),
            requests: Family::default(),
            misses: Family::default(),
            connections: Family::default(),
            miss_log: Mutex::default(),
            stall_ms: AtomicU64::new(30_000),
        }))
    }

    /// How long a mocked connection may stay silent while a request head or body is being
    /// read (the stall of an incomplete prefix, spec §4.1).
    pub fn set_stall(&self, stall: Duration) {
        let ms = u64::try_from(stall.as_millis()).unwrap_or(u64::MAX).max(1);
        self.0.stall_ms.store(ms, Ordering::Relaxed);
    }

    /// Exposes the `mimic_mock_*` metrics in `metrics`.
    pub fn register(&self, metrics: &mut Metrics) {
        let registry = metrics.registry_mut();
        registry.register(
            "mock_requests",
            "Outbound requests answered by the mock",
            self.0.requests.clone(),
        );
        registry.register(
            "mock_miss",
            "Mocked requests answered 599",
            self.0.misses.clone(),
        );
        registry.register(
            "mock_pending",
            "Mocked requests pending",
            self.0.pending.clone(),
        );
        registry.register(
            "mock_connections",
            "Outbound connections whose kind was decided in mock",
            self.0.connections.clone(),
        );
    }

    /// True while the sidecar answers outbound traffic itself.
    pub(crate) fn active(&self) -> bool {
        self.0.snapshot.mode().mode() == Mode::Mock
    }

    pub(crate) fn connection(&self, decision: &'static str) {
        self.0
            .connections
            .get_or_create(&DecisionLabel { decision })
            .inc();
    }

    /// Takes a pending slot, or `None` over the cap (one warning per streak).
    fn admit(&self) -> Option<Pending> {
        let mut slots = self.0.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.pending >= self.0.max_pending {
            if !std::mem::replace(&mut slots.overloaded, true) {
                tracing::warn!(target: "mock", limit = self.0.max_pending, "mock overload");
            }
            return None;
        }
        slots.pending += 1;
        self.0.pending.set(slots.pending as i64);
        Some(Pending(self.clone()))
    }

    /// One miss: counted, and logged once per signature and mode period.
    fn miss(&self, reason: Miss, authority: &str, method: &str, path: &str) {
        self.0
            .misses
            .get_or_create(&ReasonLabel {
                reason: reason.as_str(),
            })
            .inc();
        // The overload answer is logged as "mock overload".
        if reason == Miss::Overload {
            return;
        }
        let epoch = self.0.snapshot.mode().epoch();
        let mut log = self
            .0
            .miss_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if log.epoch != epoch {
            *log = MissLog {
                epoch,
                ..MissLog::default()
            };
        }
        let signature = (
            reason.as_str(),
            authority.to_string(),
            method.to_string(),
            path.to_string(),
        );
        if log.seen.contains(&signature) {
            return;
        }
        if log.seen.len() >= MISS_LOG_LIMIT {
            if !std::mem::replace(&mut log.limit_written, true) {
                tracing::warn!(target: "mock", "mock miss log limit reached");
            }
            return;
        }
        tracing::warn!(
            target: "mock",
            reason = reason.as_str(),
            authority,
            method,
            path,
            "mock miss"
        );
        log.seen.insert(signature);
    }
}

/// A pending slot, given back when dropped.
struct Pending(Mock);

impl Drop for Pending {
    fn drop(&mut self) {
        let inner = &self.0.0;
        let mut slots = inner.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.pending = slots.pending.saturating_sub(1);
        inner.pending.set(slots.pending as i64);
        // Back under 7/8 of the cap: the next overload is a new streak.
        if slots.overloaded && slots.pending * 8 < inner.max_pending * 7 {
            slots.overloaded = false;
            tracing::info!(target: "mock", "mock overload ended");
        }
    }
}

struct ThreadRandom;

impl Random for ThreadRandom {
    fn below(&mut self, bound: u64) -> u64 {
        rand::random_range(0..bound.max(1))
    }
}

/// When the first byte of the request being read arrived (the definition of `head_ms`, spec
/// §4.1), noted by the reads of the connection.
struct Clock {
    first: Mutex<Option<Instant>>,
    /// A request is being served: the reads are its body or the next request's lookahead.
    busy: AtomicBool,
    /// An answer was handed to hyper and not yet flushed to the client.
    owed: AtomicBool,
}

impl Clock {
    fn note(&self) {
        if !self.busy.load(Ordering::Acquire) {
            self.first
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(Instant::now);
        }
    }

    /// The first byte of the request that begins to be served.
    fn start(&self) -> Instant {
        self.busy.store(true, Ordering::Release);
        self.first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(Instant::now)
    }

    /// Whether hyper may write now: an interim or final answer of ours is due. Anything else
    /// is hyper's own answer to a request it cannot parse, which spec §8 replaces by a close.
    fn may_write(&self) -> bool {
        self.busy.load(Ordering::Acquire) || self.owed.load(Ordering::Acquire)
    }

    fn release(&self) {
        self.busy.store(false, Ordering::Release);
    }
}

/// The client side of a mocked connection, noting when bytes arrive.
struct Timed {
    inner: Stream,
    clock: Arc<Clock>,
}

impl AsyncRead for Timed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.clock.note();
        }
        polled
    }
}

impl AsyncWrite for Timed {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.clock.may_write() {
            return Poll::Ready(Ok(buf.len()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(polled, Poll::Ready(Ok(()))) && !self.clock.busy.load(Ordering::Acquire) {
            self.clock.owed.store(false, Ordering::Release);
        }
        polled
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The service of a mocked connection: one future per request.
struct Service(Arc<Conn>);

impl HyperService<Request<Incoming>> for Service {
    type Response = Response<Full<Bytes>>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        let conn = self.0.clone();
        Box::pin(async move { conn.serve(request).await })
    }
}

/// One mocked connection.
struct Conn {
    mock: Mock,
    ctx: Ctx,
    clock: Arc<Clock>,
}

/// An exchange being served. Dropped before it was answered, it is a client abort: the
/// service future is dropped when the client closes the connection.
struct Flight {
    conn: Arc<Conn>,
    authority: String,
    method: String,
    path: String,
    started: Instant,
    answered: bool,
    /// Held until the answer is returned.
    slot: Option<Pending>,
}

impl Flight {
    /// Counts the miss, and logs it with the path as the access line writes it.
    fn miss(&self, reason: Miss) {
        let path = truncate_path(&redact::path(&self.path)).to_string();
        self.conn
            .mock
            .miss(reason, &self.authority, &self.method, &path);
    }

    fn answer(
        mut self,
        response: &mock::Response,
        mock: &'static str,
        miss: Option<Miss>,
        bytes_in: u64,
    ) {
        self.answered = true;
        let head = self.started.elapsed();
        let ctx = &self.conn.ctx;
        let bytes_out = response.body().len() as u64;
        ctx.proxy.request(
            ctx.direction,
            PROTOCOL_H1,
            &self.authority,
            Some(response.status),
            head,
        );
        ctx.proxy
            .bytes(ctx.direction, PROTOCOL_H1, bytes_in, bytes_out);
        self.conn
            .mock
            .0
            .requests
            .get_or_create(&ResultLabel { result: mock })
            .inc();
        access::exchange(&access::Exchange {
            direction: ctx.direction,
            protocol: PROTOCOL_H1,
            dst: ctx.dst,
            authority: std::mem::take(&mut self.authority),
            method: Some(std::mem::take(&mut self.method)),
            path: Some(std::mem::take(&mut self.path)).filter(|p| !p.is_empty()),
            status: Some(response.status),
            grpc_status: None,
            upgrade: false,
            tls: ctx.tls,
            outcome: Outcome::Ok,
            duration: Some(head),
            head: Some(head),
            bytes_in,
            bytes_out,
            mock: Some(mock),
            mock_miss: miss.map(Miss::as_str),
        });
        self.conn.clock.owed.store(true, Ordering::Release);
        self.conn.clock.release();
        // The slot goes with the flight, at the return of the answer.
        drop(self.slot.take());
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        let ctx = &self.conn.ctx;
        ctx.proxy
            .request_unobserved(ctx.direction, PROTOCOL_H1, &self.authority);
        access::exchange(&access::Exchange {
            direction: ctx.direction,
            protocol: PROTOCOL_H1,
            dst: ctx.dst,
            authority: std::mem::take(&mut self.authority),
            method: Some(std::mem::take(&mut self.method)),
            path: Some(std::mem::take(&mut self.path)).filter(|p| !p.is_empty()),
            status: None,
            grpc_status: None,
            upgrade: false,
            tls: ctx.tls,
            outcome: Outcome::ClientAbort,
            duration: Some(self.started.elapsed()),
            head: None,
            bytes_in: 0,
            bytes_out: 0,
            mock: None,
            mock_miss: None,
        });
        self.conn.clock.release();
    }
}

/// The hyper response of a mocked answer. The body is the index's own bytes, not a copy.
fn http_response(response: &mock::Response, close: bool) -> Response<Full<Bytes>> {
    let body = response
        .shared_body()
        .filter(|b| !b.as_ref().is_empty())
        .map_or_else(Bytes::new, Bytes::from_owner);
    let mut out = Response::new(Full::new(body));
    *out.status_mut() = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (name, value) in &response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out.headers_mut().append(name, value);
        }
    }
    if close {
        out.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    out
}

impl Mock {
    /// Serves the HTTP/1 connection `stream` (the bytes read to detect it are its prefix),
    /// whose first byte arrived at `first`, until it ends. Nothing is sent to the destination.
    pub(crate) async fn serve_http1(
        &self,
        ctx: Ctx,
        stream: Stream,
        first: Instant,
        stopped: impl Future<Output = ()>,
        mut abort: Pin<&mut (impl Future<Output = ()> + Send)>,
    ) {
        let clock = Arc::new(Clock {
            first: Mutex::new(Some(first)),
            busy: AtomicBool::new(false),
            owed: AtomicBool::new(false),
        });
        let conn = Arc::new(Conn {
            mock: self.clone(),
            ctx,
            clock: clock.clone(),
        });
        let ctx = conn.ctx.clone();
        let service = Service(conn);
        let io = TokioIo::new(Timed {
            inner: stream,
            clock,
        });
        let stall = Duration::from_millis(self.0.stall_ms.load(Ordering::Relaxed));
        let served = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(stall)
            .serve_connection(io, service);
        let mut served = std::pin::pin!(served);
        let ended = tokio::select! {
            r = served.as_mut() => Some(r),
            () = stopped => {
                // The requests read are answered, then the connection closes.
                served.as_mut().graceful_shutdown();
                tokio::select! {
                    r = served.as_mut() => Some(r),
                    () = abort.as_mut() => None,
                }
            }
            () = abort.as_mut() => None,
        };
        if let Some(Err(e)) = ended {
            if e.is_parse() || e.is_parse_too_large() || e.is_parse_status() {
                self.unframed(&ctx);
            }
            tracing::debug!(target: "mock", error = %e, "mocked connection ended");
        }
    }
}

impl Mock {
    /// A request the server cannot parse: the connection was closed without an answer, and
    /// the exchange counts with `status_class="none"` (spec §8, PRX-21).
    fn unframed(&self, ctx: &Ctx) {
        let authority = ctx.dst.to_string();
        ctx.proxy
            .request_unobserved(ctx.direction, PROTOCOL_H1, &authority);
        access::exchange(&access::Exchange {
            direction: ctx.direction,
            protocol: PROTOCOL_H1,
            dst: ctx.dst,
            authority,
            method: None,
            path: None,
            status: None,
            grpc_status: None,
            upgrade: false,
            tls: ctx.tls,
            outcome: Outcome::Unframed,
            duration: None,
            head: None,
            bytes_in: 0,
            bytes_out: 0,
            mock: None,
            mock_miss: None,
        });
    }
}

impl Conn {
    async fn serve(
        self: &Arc<Conn>,
        request: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, BoxError> {
        let started = self.clock.start();
        let (parts, body) = request.into_parts();
        let target = target_authority(
            parts.uri.authority().map(|a| a.as_str()),
            parts
                .headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok()),
        );
        let ctx = &self.ctx;
        let default_port = if ctx.tls { HTTPS_PORT } else { HTTP_PORT };
        let authority = target.map_or_else(
            || ctx.dst.to_string(),
            |raw| normalise_authority(raw, default_port),
        );
        let mut flight = Flight {
            conn: self.clone(),
            authority,
            method: parts.method.as_str().to_string(),
            path: parts.uri.path().to_string(),
            started,
            answered: false,
            slot: None,
        };
        // The cap comes first: an overloaded sidecar reads nothing more of the request.
        let Some(slot) = self.mock.admit() else {
            flight.miss(Miss::Overload);
            let response = Miss::Overload.response();
            let out = http_response(&response, true);
            flight.answer(&response, "miss", Some(Miss::Overload), 0);
            return Ok(out);
        };
        flight.slot = Some(slot);

        let refused = if parts.method == Method::CONNECT {
            Some(Miss::Connect)
        } else if parts.headers.contains_key(header::UPGRADE) {
            Some(Miss::Upgrade)
        } else {
            None
        };
        if let Some(reason) = refused {
            flight.miss(reason);
            let response = reason.response();
            let out = http_response(&response, false);
            flight.answer(&response, "miss", Some(reason), 0);
            return Ok(out);
        }

        let (matched, reservation, bytes_in) = read_body(&self.mock, body).await?;
        let (response, label, miss, delay) = self.decide(&parts, target, matched.as_deref());
        // From here the answer is all that is held: the body and the index are let go before
        // the wait (spec §8 "Bounds").
        drop((matched, reservation, parts));
        if let Some(reason) = miss {
            flight.miss(reason);
        }
        if delay > 0 {
            let due = tokio::time::Instant::from_std(started) + Duration::from_millis(delay);
            tokio::time::sleep_until(due).await;
        }
        let out = http_response(&response, false);
        flight.answer(&response, label, miss, bytes_in);
        Ok(out)
    }
}

impl Conn {
    /// The answer to a request, the label of its tier or `miss`, the miss, and the delay in
    /// milliseconds. Holds the snapshot only for the time of the call.
    fn decide(
        &self,
        parts: &hyper::http::request::Parts,
        target: Option<&str>,
        body: Option<&[u8]>,
    ) -> (mock::Response, &'static str, Option<Miss>, u64) {
        let ctx = &self.ctx;
        let decoded = body.and_then(|data| {
            let encoding = parts
                .headers
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok());
            decode(data, encoding).ok()
        });
        let headers: Vec<(&str, &str)> = parts
            .headers
            .iter()
            .filter_map(|(n, v)| Some((n.as_str(), v.to_str().ok()?)))
            .collect();
        let request = mock::Request {
            method: parts.method.as_str(),
            target,
            tls: ctx.tls,
            sni: ctx.sni.as_deref(),
            dst: ctx.dst,
            h2: false,
            path: parts.uri.path(),
            query: parts.uri.query().unwrap_or_default(),
            headers: &headers,
            body: decoded.as_deref(),
        };
        let inner = &self.mock.0;
        let Some(index) = inner.snapshot.index() else {
            return (Miss::Authority.response(), "miss", Some(Miss::Authority), 0);
        };
        match index.lookup(&inner.redactor, &request) {
            Ok(hit) => {
                let factor = inner.snapshot.mode().factor();
                let delay = mock::draw(hit.latency, factor, &mut ThreadRandom);
                (
                    hit.response(parts.method.as_str()),
                    hit.tier.as_str(),
                    None,
                    delay,
                )
            }
            Err(reason) => (reason.response(), "miss", Some(reason), 0),
        }
    }
}

/// Reads the body of a request: its first 256 KiB are kept for matching when the capture
/// budget allows (else the body is matched as absent), the rest is read and dropped.
async fn read_body(
    mock: &Mock,
    mut body: Incoming,
) -> Result<(Option<Vec<u8>>, crate::discovery::Reservation, u64), BoxError> {
    let mut reservation = mock.0.budget.reservation();
    let mut kept: Vec<u8> = Vec::new();
    let mut absent = false;
    let mut total = 0u64;
    let stall = Duration::from_millis(mock.0.stall_ms.load(Ordering::Relaxed));
    loop {
        // A client that stops sending does not hold a pending slot for ever.
        let frame = match tokio::time::timeout(stall, body.frame()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request body stalled",
                )));
            }
        };
        let Ok(data) = frame.map_err(BoxError::from)?.into_data() else {
            continue;
        };
        total += data.len() as u64;
        if absent {
            continue;
        }
        if kept.len() + data.len() > MAX_MATCHED_BODY || !reservation.grow(data.len()) {
            absent = true;
            reservation.shrink(kept.len());
            kept = Vec::new();
            continue;
        }
        kept.extend_from_slice(&data);
    }
    let kept = (!absent && !kept.is_empty()).then_some(kept);
    Ok((kept, reservation, total))
}
