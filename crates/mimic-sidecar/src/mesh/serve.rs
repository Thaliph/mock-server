//! A mesh-routed connection (spec §7.9 "Load balancing", §4.1 "Mesh-routed HTTP"): the app's
//! connection is parsed by `hyper` and each request goes to a pod chosen by the balancer, over
//! a pooled connection. Observation is the same as on the byte relay: one access line, the RED
//! metrics and discovery's exchange per request, under the authority the app named. On HTTP/1
//! a head reader ([`super::gate`]) judges each request head before `hyper` sees it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::http::Extensions;
use hyper::http::request::Parts;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::fault;
use mimic_core::mesh::head::{Stop, leaves};
use mimic_core::mesh::mirror::{self, Candidate};
use mimic_core::mesh::outlier::Outcome as Counted;
use mimic_core::mesh::reason::Reason;
use mimic_core::mesh::retry::{
    Decision, Failure, Outcome, Planner, Refusal, Verdict, classify, delay, trusted,
};
use mimic_core::mesh::route::{
    FieldList, MirrorView, Pick, RequestParts, RouteView, TimeoutsView, draw, split_target,
};
use mimic_core::mesh::timeout::{self, Deadline, Kind};
use mimic_core::proxy::authority::status_class;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch};
use tokio::time::Sleep;

use super::MeshState;
use super::dial::CLUSTER_CONNECT_TIMEOUT;
use super::gate::{Gate, Progress};
use super::metrics::{Call, INVALID_BACKEND, NO_BACKEND};
use super::pool::{self, Lease, Protocol, Sent};
use super::replay::{Attempt, Held, Source, Whole};
use super::router::{Backend, InFlight, Permit, Plan, Routed, Slot};
use crate::discovery::Tap;
use crate::exchange::{self, Ctx, RespBody, Tracker};
use crate::mock::{self, Mock};
use crate::tls::intercept::{Origin, is_tls_error};
use crate::transport::Stream;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The answer to a head the reader refuses (spec §4.1 limit 1).
const REFUSED: &[u8] = b"HTTP/1.1 400 Bad Request\r\nx-mimic-error: invalid-request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
/// How long the sidecar reads and discards what the client still sends after a refusal, so
/// that closing the socket does not reset the connection before the client read the answer.
const REFUSAL_LINGER: Duration = Duration::from_millis(250);

/// What a mesh-routed connection needs.
pub(crate) struct Serving {
    pub mesh: MeshState,
    pub backend: Arc<Backend>,
    pub ctx: Ctx,
    pub tap: Option<Tap>,
    /// How TLS is spoken to the pods when the app's TLS was intercepted (spec §7.9).
    pub origin: Option<Arc<Origin>>,
    /// The mock that answers each request while the mode is `mock`; none for a passthrough
    /// destination.
    mock: Option<Mock>,
    mocked: OnceLock<Arc<mock::Conn>>,
    /// The requests the head reader gave `hyper` and those answered.
    progress: Arc<Progress>,
}

impl Serving {
    pub(crate) fn new(
        mesh: MeshState,
        backend: Arc<Backend>,
        ctx: Ctx,
        tap: Option<Tap>,
        origin: Option<Arc<Origin>>,
        mock: Option<Mock>,
    ) -> Serving {
        Serving {
            mesh,
            backend,
            ctx,
            tap,
            origin,
            mock,
            mocked: OnceLock::new(),
            progress: Arc::default(),
        }
    }

    /// Whether an `Upgrade` or `CONNECT` head leaves `hyper` now: not while the mock answers.
    fn upgrades_leave(&self) -> bool {
        !self.mock.as_ref().is_some_and(Mock::active)
    }
}

/// The connection to the pod and the request counted in flight, released when the response
/// body ends or is dropped.
struct Hold {
    _in_flight: Option<InFlight>,
    lease: Lease,
    /// The slot of the Service's breaker, held to the response's last byte.
    _permit: Option<Permit>,
}

/// A route deadline armed in the test hook's count while it lives.
struct Armed(Arc<AtomicUsize>);

impl Armed {
    fn new(count: &Arc<AtomicUsize>) -> Armed {
        count.fetch_add(1, Ordering::Relaxed);
        Armed(count.clone())
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The deadline of a response that went on after its head (spec §7.9 "Timeouts").
struct Cutoff {
    sleep: Pin<Box<Sleep>>,
    kind: Kind,
    _armed: Armed,
}

/// The error that ends the app's HTTP/1 connection, or resets its HTTP/2 stream, when a route
/// timeout expires after the response head.
#[derive(Debug)]
struct Expired(Kind);

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "route timeout {} expired", self.0.label())
    }
}

impl std::error::Error for Expired {}

/// The body of a response: the pod's, or none for an answer of the sidecar.
pub(crate) struct MeshBody {
    upstream: Option<RespBody>,
    mocked: Option<UnsyncBoxBody<Bytes, hyper::Error>>,
    hold: Option<Hold>,
    /// The timeout that cuts a response that stalls after its head.
    cutoff: Option<Cutoff>,
    done: Option<Done>,
}

impl MeshBody {
    fn empty() -> MeshBody {
        MeshBody {
            upstream: None,
            mocked: None,
            hold: None,
            cutoff: None,
            done: None,
        }
    }
}

impl Body for MeshBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let (Some(body), Some(cutoff)) = (this.upstream.as_ref(), this.cutoff.as_mut())
            && !body.is_end_stream()
            && cutoff.sleep.as_mut().poll(cx).is_ready()
        {
            // The pod's connection is never reused (an HTTP/1 one closes, an HTTP/2 stream is
            // reset by dropping it), and the app's connection or stream ends in an error.
            let kind = cutoff.kind;
            this.cutoff = None;
            if let Some(mut body) = this.upstream.take() {
                body.cut(kind);
            }
            if let Some(hold) = this.hold.as_mut() {
                hold.lease.discard();
            }
            this.hold = None;
            return Poll::Ready(Some(Err(Box::new(Expired(kind)))));
        }
        let polled = match (this.upstream.as_mut(), this.mocked.as_mut()) {
            (Some(body), _) => Pin::new(body).poll_frame(cx).map_err(BoxError::from),
            (None, Some(body)) => Pin::new(body).poll_frame(cx).map_err(BoxError::from),
            (None, None) => Poll::Ready(None),
        };
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            this.cutoff = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        match (&self.upstream, &self.mocked) {
            (Some(body), _) => body.is_end_stream(),
            (None, Some(body)) => body.is_end_stream(),
            (None, None) => true,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match (&self.upstream, &self.mocked) {
            (Some(body), _) => body.size_hint(),
            (None, Some(body)) => body.size_hint(),
            (None, None) => SizeHint::with_exact(0),
        }
    }
}

/// A failed connect to `upstream`: a handshake that did not verify (spec §7.9 "Intercepted TLS")
/// is a warning with the TLS error, any other failure is routine. `warned` is the request's:
/// only its first verification failure is a warning, the tries after it log at `debug`.
pub(super) fn connect_failed_log(upstream: &SocketAddr, e: &io::Error, warned: &AtomicBool) {
    if is_tls_error(e) && !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(%upstream, tls_error = %e, "tls verification failed");
    } else {
        tracing::debug!(%upstream, error = %e, "connect to the pod failed");
    }
}

/// How a mesh-routed connection ended.
pub(crate) enum Ended {
    Closed,
    /// An `Upgrade` or `CONNECT` request left `hyper` (spec §4.1 limit 8): the app's stream and
    /// the bytes read from the head of that request on.
    Leave {
        client: Stream,
        head: Vec<u8>,
    },
}

/// Serves `client` until it closes, `stop` completes (graceful) or `abort` completes.
pub(crate) async fn serve(
    serving: Serving,
    client: Stream,
    h2: bool,
    stop: impl Future<Output = ()> + Send,
    abort: impl Future<Output = ()> + Send,
) -> Result<Ended, hyper::Error> {
    let serving = Arc::new(serving);
    let protocol = if h2 { Protocol::Http2 } else { Protocol::Http1 };
    let service = {
        let serving = serving.clone();
        service_fn(move |req| forward(serving.clone(), req, protocol))
    };
    let mut stop = pin!(stop);
    let mut abort = pin!(abort);
    let mut stopping = false;
    if h2 {
        let connection = server::conn::http2::Builder::new(TokioExecutor::new())
            .auto_date_header(false)
            .serve_connection(TokioIo::new(client), service);
        let mut connection = pin!(connection);
        return loop {
            tokio::select! {
                served = connection.as_mut() => break served.map(|()| Ended::Closed),
                () = &mut stop, if !stopping => {
                    stopping = true;
                    connection.as_mut().graceful_shutdown();
                }
                () = &mut abort => return Ok(Ended::Closed),
            }
        };
    }
    let (gate, verdict) = Gate::new(
        client,
        {
            let serving = serving.clone();
            move || serving.upgrades_leave()
        },
        serving.progress.clone(),
    );
    let mut connection = server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .auto_date_header(false)
        .serve_connection(TokioIo::new(gate), service);
    let served = loop {
        tokio::select! {
            served = &mut connection => break served,
            () = &mut stop, if !stopping => {
                stopping = true;
                Pin::new(&mut connection).graceful_shutdown();
            }
            () = &mut abort => return Ok(Ended::Closed),
        }
    };
    let decided = verdict
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some((stop, held)) = decided else {
        return served.map(|()| Ended::Closed);
    };
    // `hyper` answered the requests before the one the reader stopped at; the socket is ours.
    let mut client = connection.into_parts().io.into_inner().into_stream();
    match stop {
        Stop::Upgrade => Ok(Ended::Leave { client, head: held }),
        Stop::Refused { request, .. } => {
            serving.refused(request.as_ref());
            let _ = client.write_all(REFUSED).await;
            let _ = client.shutdown().await;
            let mut sink = [0u8; 4096];
            let _ = tokio::time::timeout(REFUSAL_LINGER, async {
                while client.read(&mut sink).await.is_ok_and(|n| n > 0) {}
            })
            .await;
            Ok(Ended::Closed)
        }
    }
}

async fn forward(
    serving: Arc<Serving>,
    req: Request<hyper::body::Incoming>,
    protocol: Protocol,
) -> Result<Response<MeshBody>, BoxError> {
    // The request is answered when `hyper` drops the response body; an error ends the connection.
    let done = Done(serving.progress.clone());
    serving.call(req, protocol).await.map(|response| {
        response.map(|mut body| {
            body.done = Some(done);
            body
        })
    })
}

/// Marks a request answered when dropped.
struct Done(Arc<Progress>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// What the routes of a Service port decide for a request.
enum Selected<'a> {
    /// A rule matched and a backend was drawn.
    Rule {
        view: &'a RouteView,
        rule: usize,
        backend: Arc<Backend>,
    },
    /// The sidecar answers; `view` is the route that matched, if one did.
    Answer {
        view: Option<&'a RouteView>,
        rule: usize,
        status: StatusCode,
        reason: Reason,
        backend: &'static str,
    },
}

/// The field lines of a head as [`FieldList`] for a route's header modifier.
struct Fields<'a>(&'a mut HeaderMap);

impl FieldList for Fields<'_> {
    fn remove_all(&mut self, name: &str) {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            self.0.remove(name);
        }
    }

    fn append(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            self.0.append(name, value);
        }
    }
}

/// A connection for one try: the pod it goes to (counted in flight) or the original
/// destination (`pick` none).
struct Conn {
    pick: Option<InFlight>,
    lease: Lease,
}

/// How a try ended.
enum Tried {
    /// A response head came back.
    Answer {
        response: Box<Response<hyper::body::Incoming>>,
        conn: Conn,
        /// The deadline the response is read under, until its last byte.
        deadline: Option<Deadline>,
    },
    /// No response head. `error` is the failure of a request that was written.
    Failed {
        failure: Failure,
        error: Option<hyper::Error>,
        pick: Option<InFlight>,
        written: bool,
    },
    /// A route timeout expired before the response head (spec §7.9 "Timeouts").
    Timeout {
        kind: Kind,
        pick: Option<InFlight>,
        written: bool,
    },
}

/// The timeouts of the rule a request matched, and when the request began.
#[derive(Clone, Copy)]
struct Bounds<'a> {
    timeouts: Option<&'a TimeoutsView>,
    began: Instant,
    /// The deadlines armed now (test hook).
    timers: &'a Arc<AtomicUsize>,
}

impl Bounds<'_> {
    /// The milliseconds since the request began.
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// When `deadline` falls: a deadline too far to be an `Instant` is none.
    fn at(&self, deadline: Option<Deadline>) -> Option<(Instant, Kind)> {
        let d = deadline?;
        Some((
            self.began.checked_add(Duration::from_millis(d.at_ms))?,
            d.kind,
        ))
    }

    /// Runs `fut` until `deadline`: the timeout that expired when it does.
    async fn within<T>(
        &self,
        fut: impl Future<Output = T>,
        deadline: Option<Deadline>,
    ) -> Result<T, Kind> {
        match self.at(deadline) {
            None => Ok(fut.await),
            Some((at, kind)) => {
                let _armed = Armed::new(self.timers);
                tokio::time::timeout_at(at.into(), fut)
                    .await
                    .map_err(|_| kind)
            }
        }
    }

    /// The deadline of a response read after its head.
    fn cutoff(&self, deadline: Option<Deadline>) -> Option<Cutoff> {
        let (at, kind) = self.at(deadline)?;
        Some(Cutoff {
            sleep: Box::pin(tokio::time::sleep_until(at.into())),
            kind,
            _armed: Armed::new(self.timers),
        })
    }
}

impl Tried {
    /// The connect failed, or the request could not be written: nothing is on the wire.
    fn unwritten(pick: Option<InFlight>) -> Tried {
        Tried::Failed {
            failure: Failure::Connect,
            error: None,
            pick,
            written: false,
        }
    }
}

/// The middle of the delay [`delay`] draws around, to scale it by `MeshConfig.refusal_delay`.
const REFUSAL_DELAY: f64 = 0.025;
/// The body of a refused try is read and discarded up to this much (spec §7.9 "Retries").
const DISCARD_LIMIT: usize = 64 * 1024;
/// How long that takes at most before the connection is given up.
const DISCARD_TIME: Duration = Duration::from_secs(2);

impl Serving {
    /// One try on `pick` (a pod, counted in flight) or the original destination: a connection
    /// from the pool and the request on it. An unwritten request, because the pod had closed the
    /// connection, goes once more on a new one, which is part of the same try (spec §7.9).
    async fn try_once(
        &self,
        protocol: Protocol,
        pick: Option<InFlight>,
        (backend, original): (&Backend, SocketAddr),
        (source, head): (&Source, &Parts),
        warned: &AtomicBool,
        bounds: Bounds<'_>,
    ) -> Tried {
        let router = self.mesh.router();
        let upstream = pick.as_ref().map_or(original, |p| p.endpoint().addr);
        let timeout = if pick.is_some() {
            router.connect_timeout
        } else {
            CLUSTER_CONNECT_TIMEOUT
        };
        let connected = bounds
            .within(
                router.pools.checkout(
                    upstream,
                    protocol,
                    self.origin.as_ref(),
                    None,
                    (timeout, &backend.retry.max_connections()),
                ),
                timeout::call_deadline(bounds.timeouts),
            )
            .await;
        let connected = match connected {
            Ok(connected) => connected,
            Err(kind) => {
                return Tried::Timeout {
                    kind,
                    pick,
                    written: false,
                };
            }
        };
        let mut lease = match connected {
            Ok(lease) => {
                if let Some(pick) = &pick {
                    router.succeeded(pick);
                }
                lease
            }
            Err(e) => {
                connect_failed_log(&upstream, &e, warned);
                if let Some(pick) = &pick {
                    router.failed(pick);
                }
                return Tried::unwritten(pick);
            }
        };
        // The try starts when its request is handed to the connection (spec §7.9 "Timeouts").
        let deadline = timeout::try_deadline(bounds.timeouts, bounds.elapsed_ms());
        let sent = bounds
            .within(lease.send(source.request(head)), deadline)
            .await;
        let mut sent = match sent {
            Ok(sent) => sent,
            Err(kind) => return Self::timed_out(lease, pick, kind, true),
        };
        if let Sent::Unsent = sent {
            if !source.intact() {
                return Tried::Failed {
                    failure: Failure::Reset,
                    error: None,
                    pick,
                    written: true,
                };
            }
            router.metrics.reconnect(&backend.label);
            let again = bounds
                .within(
                    router.pools.checkout(
                        upstream,
                        protocol,
                        self.origin.as_ref(),
                        Some(&lease),
                        (timeout, &backend.retry.max_connections()),
                    ),
                    deadline,
                )
                .await;
            let again = match again {
                Ok(again) => again,
                Err(kind) => return Self::timed_out(lease, pick, kind, false),
            };
            match again {
                Ok(new) => lease = new,
                Err(e) => {
                    connect_failed_log(&upstream, &e, warned);
                    if let Some(pick) = &pick {
                        router.failed(pick);
                    }
                    return Tried::unwritten(pick);
                }
            }
            let resent = bounds
                .within(lease.send(source.request(head)), deadline)
                .await;
            sent = match resent {
                Ok(resent) => resent,
                Err(kind) => return Self::timed_out(lease, pick, kind, true),
            };
        }
        match sent {
            Sent::Response(response) => Tried::Answer {
                response: Box::new(response),
                conn: Conn { pick, lease },
                deadline,
            },
            Sent::Failed(e) => Tried::Failed {
                failure: pool::failure(&e, lease.goaway()),
                error: Some(e),
                pick,
                written: true,
            },
            Sent::Unsent => {
                tracing::debug!(%upstream, "the request could not be written twice");
                Tried::unwritten(pick)
            }
        }
    }

    /// A try cut by its deadline: its connection is never reused (an HTTP/1 one is closed, an
    /// HTTP/2 stream is reset by dropping it and the connection stays pooled).
    fn timed_out(mut lease: Lease, pick: Option<InFlight>, kind: Kind, written: bool) -> Tried {
        lease.discard();
        Tried::Timeout {
            kind,
            pick,
            written,
        }
    }

    /// The next try after one failed: `Stop` without one left. A stale view, or one that says
    /// `"mesh": false`, has the original destination for its only try (spec §7.9). A headless
    /// backend (`original` none) has no destination to fall back to.
    fn plan(
        &self,
        backend: &Backend,
        original: Option<SocketAddr>,
        direct: bool,
        tries: u32,
        planner: &mut Planner,
    ) -> Plan {
        if direct {
            return if tries == 0 {
                Plan::Original
            } else {
                Plan::Stop
            };
        }
        match backend.plan(self.mesh.router(), planner) {
            Plan::Original if original.is_none() => Plan::Stop,
            plan => plan,
        }
    }

    /// What to do with a failed try: a stale view, or one that says `"mesh": false`, has no
    /// try after the original destination and draws nothing on the budget.
    fn decide(
        &self,
        backend: &Backend,
        direct: bool,
        verdict: Verdict,
        planner: &Planner,
    ) -> Decision {
        if direct && verdict.retry {
            return Decision::Exhausted;
        }
        backend
            .retry
            .decide(verdict, planner, self.mesh.router().now())
    }

    /// Counts a failure that is not retried (`Retry` is handled by the caller).
    fn not_retried(&self, backend: &Backend, decision: Decision) {
        let router = self.mesh.router();
        let label = &backend.label;
        match decision {
            Decision::Skipped(skip) => router.metrics.skipped(label, skip.label()),
            Decision::Exhausted => router.metrics.skipped(label, "exhausted"),
            Decision::Budget => backend.exhausted(router),
            Decision::Done | Decision::Retry => {}
        }
    }

    /// Reads and drops the body of a refused try, so that its connection can be pooled again.
    async fn discard(response: Response<hyper::body::Incoming>, lease: &mut Lease) {
        let mut body = response.into_body();
        let mut seen = 0;
        let drained = tokio::time::timeout(DISCARD_TIME, async {
            loop {
                match body.frame().await {
                    None => return true,
                    Some(Err(_)) => return false,
                    Some(Ok(frame)) => {
                        seen += frame.data_ref().map_or(0, Bytes::len);
                        if seen > DISCARD_LIMIT {
                            return false;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or(false);
        if !drained {
            lease.discard();
        }
    }

    /// A head the reader refused: the access line and the count (the `400` itself is written
    /// by the caller).
    fn refused(&self, request: Option<&mimic_core::proxy::http1::RequestHead>) {
        exchange::refused(&self.ctx, request);
        self.count(
            "none",
            &self.backend.label,
            StatusCode::BAD_REQUEST,
            Some(Reason::InvalidRequest),
            Instant::now(),
        );
    }

    async fn call(
        &self,
        req: Request<hyper::body::Incoming>,
        protocol: Protocol,
    ) -> Result<Response<MeshBody>, BoxError> {
        let http1 = protocol == Protocol::Http1;
        let label = if http1 { "http1" } else { "h2" };
        // The mode applies per request: while it is `mock` the mock answers, as on the byte
        // relay (spec §7.9).
        if let Some(mock) = self.mock.as_ref().filter(|m| m.active()) {
            let conn = self
                .mocked
                .get_or_init(|| mock.switched_mesh(self.ctx.clone(), !http1));
            let answered = conn.serve(req).await?;
            return Ok(answered.map(|body| MeshBody {
                upstream: None,
                mocked: Some(body.map_err(|never| match never {}).boxed_unsync()),
                hold: None,
                cutoff: None,
                done: None,
            }));
        }
        let began = Instant::now();
        // An HTTP/1 `Upgrade` or `CONNECT` leaves `hyper` before it gets here (the head
        // reader); one that does is after a request the reader could not follow. HTTP/2
        // `CONNECT` is not supported (spec §4.1 limit 8).
        if req.method() == Method::CONNECT || (http1 && req.headers().contains_key(header::UPGRADE))
        {
            let (tracker, _) = Tracker::start(&self.ctx, label, self.tap.as_ref(), req);
            return Ok(self.own_answer(
                tracker,
                ("none", &self.backend.label),
                (StatusCode::NOT_IMPLEMENTED, Reason::Unsupported),
                http1,
                began,
            ));
        }
        let mut req = req;
        if http1 {
            strip_hop_by_hop(req.headers_mut(), true);
        }
        let (mut tracker, req) = Tracker::start(&self.ctx, label, self.tap.as_ref(), req);
        let router = self.mesh.router();
        let (mut head, body) = req.into_parts();
        let body_state = body_state(&head.headers, body.is_end_stream());
        let source = Source::new(body, self.mesh.replay());
        let direct = self.mesh.stale() || self.mesh.off();
        // The routes of the port now (spec §7.9 "Routing"): none for a stale view.
        let routed = if direct { None } else { self.backend.routed() };
        // The request as the app sent it, for the route match and the fault decision: made once,
        // and dropped before the request modifier changes its fields.
        let fields = (!direct && (routed.is_some() || self.backend.retry.has_faults()))
            .then(|| field_list(&head));
        let parts = fields.as_deref().map(|fields| request_parts(&head, fields));
        let mut route = "none";
        let mut drawn = self.backend.clone();
        // Where the fallback goes: the ClusterIP of the drawn backend (none when it is headless),
        // else the destination the app dialled.
        let mut original = Some(self.ctx.dst);
        let mut response_modifier = None;
        let mut timeouts = None;
        // The copies wait for it: it opens at the primary's response head, or when the call
        // ends without one (spec §7.9 "Mirrors", "Start").
        let mut gate = None;
        // The breaker's slot, held from the first try to the response's end (spec §7.9).
        let mut permit: Option<Permit> = None;
        // The delay drawn, waited once the slot is taken (spec §7.9 "Fault injection").
        let mut delay_ms = None;
        // An abort closes an HTTP/1 connection whose request body is not read whole.
        let close = || http1 && !source.complete();
        if let (Some(routed), Some(parts)) = (&routed, &parts) {
            match self.select(routed, parts) {
                Selected::Rule {
                    view,
                    rule,
                    backend,
                } => {
                    route = &view.name;
                    tracker.matched(route, rule);
                    // An abort comes before the breaker's slot (spec §7.9 "Order").
                    match self.fault_decision(&backend, parts) {
                        Ok(ms) => delay_ms = ms,
                        Err(status) => {
                            return Ok(self.aborted(
                                tracker,
                                route,
                                &backend,
                                status,
                                close(),
                                began,
                            ));
                        }
                    }
                    match backend.admit(router) {
                        Ok(taken) => permit = Some(taken),
                        Err(()) => {
                            return Ok(self.open(tracker, route, &backend, began));
                        }
                    }
                    let filters = view.rules[rule].filters.as_ref();
                    let mirrors = filters.map_or(&[][..], |f| &f.mirrors[..]);
                    // A mirror written before the request modifier gets the app's fields.
                    let as_sent = mirrors
                        .iter()
                        .any(|m| m.before_modifier)
                        .then(|| head.headers.clone());
                    if let Some(modifier) = filters.and_then(|f| f.request_headers.as_ref()) {
                        modifier.apply(&mut Fields(&mut head.headers));
                    }
                    gate = self.mirror(
                        mirrors,
                        (&head, as_sent.as_ref()),
                        (body_state, &source),
                        (view.rules[rule].timeouts.as_ref(), began),
                        (protocol, &tracker.mirrored()),
                    );
                    response_modifier = filters.and_then(|f| f.response_headers.as_ref());
                    timeouts = view.rules[rule].timeouts.as_ref();
                    if !Arc::ptr_eq(&backend, &self.backend) {
                        original = backend.original();
                    }
                    drawn = backend;
                }
                Selected::Answer {
                    view,
                    rule,
                    status,
                    reason,
                    backend,
                } => {
                    if let Some(view) = view {
                        route = &view.name;
                        tracker.matched(route, rule);
                    }
                    return Ok(self.own_answer(
                        tracker,
                        (route, backend),
                        (status, reason),
                        false,
                        began,
                    ));
                }
            }
        } else if !direct {
            let decided = parts
                .as_ref()
                .map_or(Ok(None), |parts| self.fault_decision(&drawn, parts));
            match decided {
                Ok(ms) => delay_ms = ms,
                Err(status) => {
                    return Ok(self.aborted(tracker, route, &drawn, status, close(), began));
                }
            }
            match drawn.admit(router) {
                Ok(taken) => permit = Some(taken),
                Err(()) => return Ok(self.open(tracker, route, &drawn, began)),
            }
        }
        let backend = &drawn.label;
        let bounds = Bounds {
            timeouts,
            began,
            timers: &router.timers,
        };
        // The delay holds the slot, before the first try is planned: once per request, whatever
        // its tries (spec §7.9 "Fault injection").
        if let Some(ms) = delay_ms {
            if !drawn.has_endpoints() {
                return Ok(self.no_endpoint(tracker, route, backend, began));
            }
            router.metrics.fault(backend, "delay");
            tracker.backend(backend);
            tracker.fault(fault::Kind::Delay { ms });
            if let Err(kind) = self.delay(ms, bounds).await {
                let upstream = original.unwrap_or(self.ctx.dst);
                return Ok(self.cut(
                    tracker,
                    &source,
                    (route, backend, upstream),
                    (kind, protocol),
                    began,
                ));
            }
        }
        let mut planner = Planner::new(drawn.retry.retries());
        drawn.retry.deposit(router.now());
        let warned = AtomicBool::new(false);
        let mut tries = 0;
        let mut again: Option<Outcome> = None;
        let mut next = self.plan(&drawn, original, direct, tries, &mut planner);
        loop {
            let pick = match next {
                Plan::Pod(pick) => Some(pick),
                Plan::Original => None,
                Plan::AllEjected => {
                    // Every endpoint was ejected between the check and the plan.
                    drawn.opened(router, "all_ejected", 0);
                    return Ok(self.open(tracker, route, &drawn, began));
                }
                Plan::Stop => return Ok(self.no_endpoint(tracker, route, backend, began)),
            };
            tries += 1;
            if let Some(reason) = again.take() {
                router.metrics.retry(backend, reason);
            }
            if pick.is_none() {
                router.metrics.fallback(backend);
            }
            // `plan` gives no original destination to a headless backend.
            let dst = original.unwrap_or(self.ctx.dst);
            let upstream = pick.as_ref().map_or(dst, |p| p.endpoint().addr);
            tracker.route(backend, upstream, tries);
            match self
                .try_once(
                    protocol,
                    pick,
                    (&drawn, dst),
                    (&source, &head),
                    &warned,
                    bounds,
                )
                .await
            {
                Tried::Answer {
                    mut response,
                    mut conn,
                    deadline,
                } => {
                    let status = response.status();
                    let refusal = refusal_of(&response, conn.pick.as_ref(), self.origin.is_some());
                    let failure = refusal.map_or(Failure::Response, Failure::Refusal);
                    let verdict = classify(failure, head.method.as_str(), source.held(), true);
                    router.metrics.attempt(backend, verdict.outcome);
                    if let Some(pick) = &conn.pick {
                        let counted = match refusal {
                            Some(Refusal::Overloaded) => Counted::Overloaded,
                            Some(Refusal::QueueTimeout) => Counted::QueueTimeout,
                            Some(Refusal::AppRefused) => Counted::AppRefused,
                            None => Counted::Head(status.as_u16()),
                        };
                        drawn.record(router, pick.endpoint().addr, counted);
                        let class = status_class(status.as_u16());
                        router
                            .metrics
                            .endpoint_try(backend, &pick.endpoint().label, class);
                    }
                    planner.failed(verdict.outcome);
                    match self.decide(&drawn, direct, verdict, &planner) {
                        Decision::Retry => {
                            match self.plan(&drawn, original, direct, tries, &mut planner) {
                                Plan::Stop | Plan::AllEjected => {
                                    router.metrics.skipped(backend, "exhausted")
                                }
                                // The try in flight stops reading the app's body now, so that
                                // what the next try replays is what was read.
                                retry if source.detach() => {
                                    // The refusal is read and the pause kept under the call's
                                    // deadline: the route's promise wins over the retry.
                                    let call = timeout::call_deadline(timeouts);
                                    let drained = bounds
                                        .within(Self::discard(*response, &mut conn.lease), call)
                                        .await;
                                    if let Err(kind) = drained {
                                        conn.lease.discard();
                                        drop(conn);
                                        return Ok(self.cut(
                                            tracker,
                                            &source,
                                            (route, backend, upstream),
                                            (kind, protocol),
                                            began,
                                        ));
                                    }
                                    drop(conn);
                                    if let Err(kind) =
                                        bounds.within(self.pause(verdict.outcome), call).await
                                    {
                                        return Ok(self.cut(
                                            tracker,
                                            &source,
                                            (route, backend, upstream),
                                            (kind, protocol),
                                            began,
                                        ));
                                    }
                                    again = Some(verdict.outcome);
                                    next = retry;
                                    continue;
                                }
                                _ => router.metrics.skipped(backend, "buffer"),
                            }
                        }
                        other => self.not_retried(&drawn, other),
                    }
                    source.release();
                    drop(gate.take());
                    let Conn { pick, lease } = conn;
                    if let Some(modifier) = response_modifier {
                        modifier.apply(&mut Fields(response.headers_mut()));
                    }
                    let answered = match refusal {
                        Some(_) => tracker.answered_unrecorded(*response),
                        None => tracker.answered(*response),
                    };
                    let mut answered = answered;
                    if http1 {
                        strip_hop_by_hop(answered.headers_mut(), false);
                    }
                    let reason = refusal.map(Refusal::reason);
                    self.count(route, backend, answered.status(), reason, began);
                    return Ok(answered.map(|body| MeshBody {
                        upstream: Some(body),
                        mocked: None,
                        hold: Some(Hold {
                            _in_flight: pick,
                            lease,
                            _permit: permit.take(),
                        }),
                        cutoff: bounds.cutoff(deadline),
                        done: None,
                    }));
                }
                Tried::Timeout {
                    kind,
                    pick,
                    written,
                } => {
                    // Never retried, and nothing asked of the budget (spec §7.9 "Timeouts"); the
                    // pod is not avoided.
                    router.metrics.attempt(backend, Outcome::Timeout);
                    if let (true, Some(pick)) = (written, &pick) {
                        let label = &pick.endpoint().label;
                        router.metrics.endpoint_try(backend, label, "none");
                    }
                    drop(pick);
                    return Ok(self.cut(
                        tracker,
                        &source,
                        (route, backend, upstream),
                        (kind, protocol),
                        began,
                    ));
                }
                Tried::Failed {
                    failure,
                    error,
                    pick,
                    written,
                } => {
                    let verdict = classify(failure, head.method.as_str(), source.held(), written);
                    router.metrics.attempt(backend, verdict.outcome);
                    if let Some(pick) = &pick {
                        let counted = match failure {
                            Failure::Connect => Counted::ConnectFailed,
                            Failure::RefusedStream => Counted::RefusedStream,
                            Failure::Response => Counted::InvalidResponse,
                            // The app leaving is not the pod's doing.
                            Failure::Reset if source.failed() => Counted::AppLeft,
                            Failure::Reset => Counted::Reset,
                            Failure::Refusal(_) | Failure::AfterHead | Failure::Timeout => {
                                Counted::AfterHead
                            }
                        };
                        drawn.record(router, pick.endpoint().addr, counted);
                    }
                    if let (true, Some(pick)) = (written, &pick) {
                        let label = &pick.endpoint().label;
                        router.metrics.endpoint_try(backend, label, "none");
                    }
                    planner.failed(verdict.outcome);
                    match self.decide(&drawn, direct, verdict, &planner) {
                        Decision::Retry => {
                            match self.plan(&drawn, original, direct, tries, &mut planner) {
                                Plan::Stop | Plan::AllEjected => {
                                    router.metrics.skipped(backend, "exhausted")
                                }
                                retry if source.detach() => {
                                    drop(pick);
                                    again = Some(verdict.outcome);
                                    next = retry;
                                    continue;
                                }
                                _ => router.metrics.skipped(backend, "buffer"),
                            }
                        }
                        other => self.not_retried(&drawn, other),
                    }
                    return self.failed(tracker, (route, backend), failure, error, upstream, began);
                }
            }
        }
    }

    /// The breaker is open: the `503` `breaker-open` of `backend`, counted and logged, with no
    /// try and nothing asked of the budget (spec §7.9 "Circuit breaker").
    fn open(
        &self,
        mut tracker: Tracker,
        route: &str,
        backend: &Backend,
        began: Instant,
    ) -> Response<MeshBody> {
        tracker.backend(&backend.label);
        self.own_answer(
            tracker,
            (route, &backend.label),
            (StatusCode::SERVICE_UNAVAILABLE, Reason::BreakerOpen),
            false,
            began,
        )
    }

    /// The fault of the request (spec §7.9 "Fault injection"), with the faults of the drawn
    /// backend's policy: `Ok` with the delay's milliseconds, if any, or `Err` with the abort's
    /// status. None at once when it has none, so that no random value is drawn; the abort's value
    /// is drawn before the delay's.
    fn fault_decision(
        &self,
        drawn: &Backend,
        parts: &RequestParts<'_>,
    ) -> Result<Option<u64>, u16> {
        let faults = drawn.retry.faults();
        if faults.is_empty() {
            return Ok(None);
        }
        let router = self.mesh.router();
        match faults
            .decide(parts, router.random(), router.random())
            .map(|d| d.kind)
        {
            Some(fault::Kind::Abort { status }) => Err(status),
            Some(fault::Kind::Delay { ms }) => Ok(Some(ms)),
            None => Ok(None),
        }
    }

    /// An abort (spec §7.9 "Fault injection"): the configured `status` of `backend`, counted
    /// and logged, with no try, no slot of the breaker and nothing asked of the budget.
    fn aborted(
        &self,
        mut tracker: Tracker,
        route: &str,
        backend: &Backend,
        status: u16,
        close: bool,
        began: Instant,
    ) -> Response<MeshBody> {
        // The view holds statuses 200 to 599 only.
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
        self.mesh.router().metrics.fault(&backend.label, "abort");
        tracker.backend(&backend.label);
        tracker.fault(fault::Kind::Abort {
            status: status.as_u16(),
        });
        self.own_answer(
            tracker,
            (route, &backend.label),
            (status, Reason::FaultInjected),
            close,
            began,
        )
    }

    /// Waits an injected delay (spec §7.9 "Fault injection"): `Err` with the route timeout that
    /// expired first. Its timer counts in the test hook while it lives; it ends with the call when
    /// the app leaves, and `SIGTERM` does not shorten it.
    async fn delay(&self, ms: u64, bounds: Bounds<'_>) -> Result<(), Kind> {
        let wait = async {
            let _armed = Armed::new(bounds.timers);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        };
        bounds
            .within(wait, timeout::call_deadline(bounds.timeouts))
            .await
    }

    /// The backend has no endpoint: the `503` `no-endpoint`, counted and logged.
    fn no_endpoint(
        &self,
        tracker: Tracker,
        route: &str,
        backend: &str,
        began: Instant,
    ) -> Response<MeshBody> {
        self.own_answer(
            tracker,
            (route, backend),
            (StatusCode::SERVICE_UNAVAILABLE, Reason::NoEndpoint),
            false,
            began,
        )
    }

    /// The call ends on a route timeout before the response head: the `504`, counted and
    /// logged (spec §7.9 "Timeouts").
    fn cut(
        &self,
        tracker: Tracker,
        source: &Source,
        (route, backend, upstream): (&str, &str, SocketAddr),
        (kind, protocol): (Kind, Protocol),
        began: Instant,
    ) -> Response<MeshBody> {
        // The request body is read to its end, or the app's connection ends with the answer.
        let http1 = protocol == Protocol::Http1;
        let close = http1 && !source.complete();
        source.cancel();
        tracing::debug!(%upstream, timeout = kind.label(), "the call timed out");
        self.count(
            route,
            backend,
            StatusCode::GATEWAY_TIMEOUT,
            Some(Reason::Timeout),
            began,
        );
        tracker.timed_out(kind);
        answer(StatusCode::GATEWAY_TIMEOUT, Reason::Timeout, close)
    }

    /// The rule of `routed` that matches the request and its drawn backend, or the answer of
    /// the sidecar (spec §7.9 "Routing").
    fn select<'a>(&self, routed: &'a Routed, parts: &RequestParts<'_>) -> Selected<'a> {
        let Some((route, rule)) = routed.table.select(parts) else {
            return Selected::Answer {
                view: None,
                rule: 0,
                status: StatusCode::NOT_FOUND,
                reason: Reason::NoRoute,
                backend: NO_BACKEND,
            };
        };
        let view = &routed.routes[route];
        let backends = &view.rules[rule].backends;
        let invalid = |reason, backend| Selected::Answer {
            view: Some(view),
            rule,
            status: StatusCode::INTERNAL_SERVER_ERROR,
            reason,
            backend,
        };
        match draw(backends, self.mesh.router().random()) {
            Pick::NoBackend => invalid(Reason::NoBackend, NO_BACKEND),
            Pick::Invalid => invalid(Reason::InvalidBackend, INVALID_BACKEND),
            Pick::Backend(i) => {
                let b = &backends[i];
                let found = b
                    .service
                    .as_deref()
                    .zip(b.port)
                    .and_then(|(service, port)| self.mesh.router().backend(service, port));
                match found {
                    Some(backend) => Selected::Rule {
                        view,
                        rule,
                        backend,
                    },
                    // A backend the view does not hold is as invalid as one it names so.
                    None => invalid(Reason::InvalidBackend, INVALID_BACKEND),
                }
            }
        }
    }

    /// Decides the copies of the request for the `mirrors` of its rule and spawns their tasks
    /// (spec §7.9 "Mirrors"). Nothing here waits: the copies start when the returned [`MirrorGate`]
    /// opens. `sent` is set when a copy is sure to be sent: when the gate opens for a request
    /// without a body (before its deadline), else by the copy once it has its body.
    fn mirror(
        &self,
        mirrors: &[MirrorView],
        (head, as_sent): (&Parts, Option<&HeaderMap>),
        (body, source): (mirror::Body, &Source),
        (timeouts, began): (Option<&TimeoutsView>, Instant),
        (protocol, sent): (Protocol, &Arc<AtomicBool>),
    ) -> Option<MirrorGate> {
        // No copy starts after `SIGTERM`: none is decided, so none is claimed as sent.
        if mirrors.is_empty() || self.mesh.stopping() {
            return None;
        }
        let router = self.mesh.router();
        let labels: Vec<String> = mirrors
            .iter()
            .map(|m| format!("{}:{}", m.service, m.port))
            .collect();
        // Each mirror takes its own random value, drawn or not.
        let candidates: Vec<Candidate<'_>> = mirrors
            .iter()
            .zip(&labels)
            .map(|(m, port)| Candidate {
                port,
                drawn: mirror::drawn(m.numerator, m.denominator, router.random()),
            })
            .collect();
        let share = u64::try_from(self.mesh.replay_share()).unwrap_or(u64::MAX);
        let timeout = u64::try_from(router.mirror_timeout.as_millis()).unwrap_or(u64::MAX);
        let ms = mirror::deadline_ms(timeout, timeouts);
        let deadline = began
            .checked_add(Duration::from_millis(ms))
            .unwrap_or_else(|| Instant::now() + router.mirror_timeout);
        let decided = router.mirror_decide(&candidates, body, share);
        let (open, go) = watch::channel(());
        let (mut bodiless, mut awaiting) = (false, false);
        for ((m, label), decision) in mirrors.iter().zip(&labels).zip(decided) {
            let Some((decision, slot)) = decision else {
                continue;
            };
            let result = match (decision, slot) {
                (decision @ (mirror::Decision::Send | mirror::Decision::AwaitBody), Some(slot)) => {
                    // The mirror left the view since the decision: no request is made, the
                    // copy counts as `error` (spec: "sent or error").
                    let Some(backend) = router.backend(&m.service, m.port) else {
                        router.metrics.mirrored(label, "error");
                        sent.store(true, Ordering::SeqCst);
                        continue;
                    };
                    // A body is waited for by the copy, in its own task, never by the request.
                    let body = (decision == mirror::Decision::AwaitBody).then(|| source.defer());
                    bodiless |= decision == mirror::Decision::Send;
                    awaiting |= decision == mirror::Decision::AwaitBody;
                    let copy = Copy {
                        body,
                        go: go.clone(),
                        sent: sent.clone(),
                        mesh: self.mesh.clone(),
                        backend,
                        protocol,
                        origin: self.origin.clone(),
                        head: CopyHead::of(
                            head,
                            as_sent
                                .filter(|_| m.before_modifier)
                                .unwrap_or(&head.headers),
                        ),
                        deadline,
                        _slot: slot,
                    };
                    tokio::spawn(copy.run());
                    continue;
                }
                (mirror::Decision::SkipLimit, _) => "skipped_limit",
                _ => "skipped_body",
            };
            router.metrics.mirrored(label, result);
        }
        Some(MirrorGate {
            open: Some(open),
            sent: sent.clone(),
            bodiless,
            body: awaiting.then(|| source.whole()),
            deadline,
        })
    }

    /// The delay before the try that follows a try that ended with `outcome`.
    async fn pause(&self, outcome: Outcome) {
        let router = self.mesh.router();
        let pause = delay(outcome, router.unit())
            .mul_f64(router.refusal_delay.as_secs_f64() / REFUSAL_DELAY);
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }

    /// The request ends without a response: the answer of the sidecar, or the error that ends
    /// the app's connection or stream. `labels` are the `route` and `backend` it is counted under.
    fn failed(
        &self,
        tracker: Tracker,
        (route, backend): (&str, &str),
        failure: Failure,
        error: Option<hyper::Error>,
        upstream: SocketAddr,
        began: Instant,
    ) -> Result<Response<MeshBody>, BoxError> {
        match failure {
            Failure::Connect => {
                tracing::debug!(%upstream, "connect failed");
                Ok(self.own_answer(
                    tracker,
                    (route, backend),
                    (StatusCode::SERVICE_UNAVAILABLE, Reason::ConnectFailed),
                    false,
                    began,
                ))
            }
            Failure::Response => {
                tracing::debug!(%upstream, "the pod's response head was refused");
                Ok(self.own_answer(
                    tracker,
                    (route, backend),
                    (StatusCode::BAD_GATEWAY, Reason::InvalidResponse),
                    false,
                    began,
                ))
            }
            _ => {
                tracing::debug!(%upstream, "the pod did not answer");
                self.count_none(route, backend, began);
                tracker.failed();
                // The app's stream is reset with the code the pod used, or `REFUSED_STREAM`
                // when it refused the stream by a `GOAWAY`.
                if failure == Failure::RefusedStream {
                    return Err(h2::Error::from(h2::Reason::REFUSED_STREAM).into());
                }
                Err(error.map_or_else(|| "connection closed".into(), Into::into))
            }
        }
    }

    /// One of the sidecar's own answers (`x-mimic-error`, no body): counted, logged and
    /// written; with `close` the connection ends.
    fn own_answer(
        &self,
        tracker: Tracker,
        (route, backend): (&str, &str),
        (status, reason): (StatusCode, Reason),
        close: bool,
        began: Instant,
    ) -> Response<MeshBody> {
        self.count(route, backend, status, Some(reason), began);
        tracker.synthetic(status.as_u16(), reason);
        answer(status, reason, close)
    }

    fn count(
        &self,
        route: &str,
        backend: &str,
        status: StatusCode,
        reason: Option<Reason>,
        began: Instant,
    ) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                route,
                backend,
                status_class: status_class(status.as_u16()),
                reason,
            },
            began.elapsed(),
        );
    }

    /// A request that got no response.
    fn count_none(&self, route: &str, backend: &str, began: Instant) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                route,
                backend,
                status_class: "none",
                reason: None,
            },
            began.elapsed(),
        );
    }
}

/// The body of a request as its head tells it (spec §7.9 "Mirrors").
fn body_state(headers: &HeaderMap, ended: bool) -> mirror::Body {
    if ended {
        return mirror::Body::None;
    }
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(mirror::Body::Unknown, mirror::Body::Declared)
}

/// The head of a copy: what the primary's first try sends, or what the app sent.
struct CopyHead {
    method: Method,
    uri: hyper::Uri,
    version: hyper::Version,
    headers: HeaderMap,
    extensions: Extensions,
}

impl CopyHead {
    fn of(head: &Parts, headers: &HeaderMap) -> CopyHead {
        CopyHead {
            method: head.method.clone(),
            uri: head.uri.clone(),
            version: head.version,
            headers: headers.clone(),
            extensions: head.extensions.clone(),
        }
    }

    fn request(&self, body: Option<&Held>) -> Request<Attempt> {
        let mut request = Request::new(body.map_or_else(Attempt::bodiless, Held::attempt));
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers.clone();
        *request.extensions_mut() = self.extensions.clone();
        request
    }
}

/// What holds the copies of a request back until the primary's response head arrived or the call
/// ended without one (spec §7.9 "Mirrors", "Start"): dropping it, when the call ends or its
/// future is dropped (the client left), opens it as well.
struct MirrorGate {
    open: Option<watch::Sender<()>>,
    /// Set when it opens before the deadline and a copy without a body, or one that already has
    /// its body, is then sent (or fails).
    sent: Arc<AtomicBool>,
    bodiless: bool,
    body: Option<Whole>,
    deadline: Instant,
}

impl Drop for MirrorGate {
    fn drop(&mut self) {
        let ready = self.bodiless || self.body.as_ref().is_some_and(Whole::given);
        if ready && Instant::now() < self.deadline {
            self.sent.store(true, Ordering::SeqCst);
        }
        self.open.take();
    }
}

/// A copy of a request on its way to a pod of a mirror (spec §7.9 "Mirrors"): one try, in its
/// own task and on pools of its own, never awaited by the request. It holds its slot until it
/// ends.
struct Copy {
    mesh: MeshState,
    backend: Arc<Backend>,
    protocol: Protocol,
    origin: Option<Arc<Origin>>,
    head: CopyHead,
    /// The body, when the request has one: it comes once its end was read from the app, whole,
    /// or never (`None`).
    body: Option<oneshot::Receiver<Option<Held>>>,
    /// Opens at the primary's response head, or at its end without one.
    go: watch::Receiver<()>,
    /// Set once a copy with a body has it and the primary's head is there.
    sent: Arc<AtomicBool>,
    /// When the copy is given up, wherever it is.
    deadline: Instant,
    _slot: Slot,
}

impl Copy {
    async fn run(mut self) {
        let router = self.mesh.router();
        let _armed = Armed::new(&router.timers);
        let at = tokio::time::Instant::from(self.deadline);
        let held = match self.body.take() {
            None => None,
            Some(rx) => match tokio::time::timeout_at(at, rx).await {
                Ok(Ok(Some(held))) => Some(held),
                // Too large, cut, gone or not ended at the deadline.
                _ => {
                    if !self.mesh.stopping() {
                        router.metrics.mirrored(&self.backend.label, "skipped_body");
                    }
                    return;
                }
            },
        };
        // Not before the primary's head; the sender is dropped when the call ends.
        if tokio::time::timeout_at(at, self.go.changed())
            .await
            .is_err()
        {
            if !self.mesh.stopping() {
                router.metrics.mirrored(&self.backend.label, "error");
            }
            return;
        }
        if held.is_some() && Instant::now() < self.deadline {
            self.sent.store(true, Ordering::SeqCst);
        }
        // No copy starts after `SIGTERM`; those in flight are dropped with the process.
        if self.mesh.stopping() {
            return;
        }
        let result = self.attempt(held.as_ref()).await;
        router.metrics.mirrored(&self.backend.label, result);
    }

    /// `sent` when a response head came back, else `error`.
    async fn attempt(&self, held: Option<&Held>) -> &'static str {
        let router = self.mesh.router();
        let at = tokio::time::Instant::from(self.deadline);
        let Plan::Pod(pick) = self.backend.plan(router, &mut Planner::new(0)) else {
            return "error";
        };
        let backend = &self.backend;
        let upstream = pick.endpoint().addr;
        let mut lease: Option<Lease> = None;
        // An unwritten request, because the pod had closed the connection, goes once more on a
        // new one: the same try.
        let response = loop {
            let connected = tokio::time::timeout_at(
                at,
                router.mirror_pools.checkout(
                    upstream,
                    self.protocol,
                    self.origin.as_ref(),
                    lease.as_ref(),
                    (
                        router.connect_timeout,
                        &self.backend.retry.max_connections(),
                    ),
                ),
            )
            .await;
            let mut connected = match connected {
                Ok(Ok(connected)) => {
                    router.succeeded(&pick);
                    connected
                }
                Ok(Err(e)) => {
                    tracing::debug!(%upstream, error = %e, "connect to the mirror's pod failed");
                    router.failed(&pick);
                    backend.record(router, upstream, Counted::ConnectFailed);
                    return "error";
                }
                Err(_) => return "error",
            };
            let sent = tokio::time::timeout_at(at, connected.send(self.head.request(held))).await;
            match sent {
                Err(_) => {
                    connected.discard();
                    return "error";
                }
                Ok(Sent::Response(response)) => {
                    // A copy's status is never read: a head says the pod answers.
                    backend.record(router, upstream, Counted::Head(200));
                    lease = Some(connected);
                    break response;
                }
                Ok(Sent::Unsent) if lease.is_none() => lease = Some(connected),
                Ok(Sent::Unsent) => {
                    backend.record(router, upstream, Counted::ConnectFailed);
                    return "error";
                }
                Ok(Sent::Failed(e)) => {
                    let counted = match pool::failure(&e, connected.goaway()) {
                        Failure::RefusedStream => Counted::RefusedStream,
                        Failure::Response => Counted::InvalidResponse,
                        _ => Counted::Reset,
                    };
                    backend.record(router, upstream, counted);
                    return "error";
                }
            }
        };
        // The answer is read and dropped as it comes; one that is not over at the deadline
        // costs the connection.
        let mut body = response.into_body();
        let drained = tokio::time::timeout_at(at, async {
            while let Some(frame) = body.frame().await {
                if frame.is_err() {
                    return false;
                }
            }
            true
        })
        .await;
        if !matches!(drained, Ok(true))
            && let Some(lease) = lease.as_mut()
        {
            lease.discard();
        }
        "sent"
    }
}

/// Whether the answer is a trusted refusal of the pod the try went to (spec §7.9 "Retries").
fn refusal_of(
    response: &Response<hyper::body::Incoming>,
    pick: Option<&InFlight>,
    tls: bool,
) -> Option<Refusal> {
    // Over intercepted TLS the callee's sidecar cannot strip the fields (spec §7.9 "Retries").
    if tls {
        return None;
    }
    let pod = pick?.endpoint().addr;
    if response.status() != StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let headers = response.headers();
    let fields: Vec<(&[u8], &[u8])> = headers
        .get_all("x-mimic-error")
        .iter()
        .map(|v| (&b"x-mimic-error"[..], v.as_bytes()))
        .chain(
            headers
                .get_all("x-mimic-endpoint")
                .iter()
                .map(|v| (&b"x-mimic-endpoint"[..], v.as_bytes())),
        )
        .chain(
            headers
                .get_all("x-mimic-origin")
                .iter()
                .map(|v| (&b"x-mimic-origin"[..], v.as_bytes())),
        )
        .collect();
    trusted(response.status().as_u16(), &fields, Some(pod))
}

/// The fields of a request head, in the order received, for the route match and the fault
/// decision.
fn field_list(head: &Parts) -> Vec<(&str, &[u8])> {
    head.headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect()
}

/// What the route match and the fault decision see of a request: its method, target and
/// `fields`.
fn request_parts<'a>(head: &'a Parts, fields: &'a [(&'a str, &'a [u8])]) -> RequestParts<'a> {
    let (path, query) = split_target(head.uri.path_and_query().map_or("", |p| p.as_str()));
    RequestParts {
        method: head.method.as_str(),
        path,
        query,
        headers: fields,
    }
}

/// An answer of the sidecar: `x-mimic-error` and no body. With `close` the connection ends.
fn answer(status: StatusCode, reason: Reason, close: bool) -> Response<MeshBody> {
    let mut response = Response::new(MeshBody::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("x-mimic-error", HeaderValue::from_static(reason.as_str()));
    // A `204` or a `304` has no content and no `content-length` (RFC 9110 §8.6).
    if status != StatusCode::NO_CONTENT && status != StatusCode::NOT_MODIFIED {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    }
    if close {
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Removes the fields that concern one hop (RFC 9110 §7.6.1): `Connection` and the fields it
/// names, `Keep-Alive`, `Proxy-Connection`, and `TE` but for `trailers` on a request. The
/// fields that frame or address the message are never removed by a `Connection` naming them.
fn strip_hop_by_hop(headers: &mut HeaderMap, request: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    for name in named {
        if ![
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::HOST,
        ]
        .contains(&name)
        {
            headers.remove(name);
        }
    }
    headers.remove(header::CONNECTION);
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
    let trailers = request
        && headers
            .get_all(header::TE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("trailers"));
    headers.remove(header::TE);
    if trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
}

/// What the first request head of a connection says about its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Head {
    /// The head is not whole yet.
    Partial,
    /// An `Upgrade` or a `CONNECT`: the connection stays on the byte relay.
    Relay,
    /// An ordinary request: the connection is balanced.
    Routed,
}

/// Looks at the first request head of a connection in `buf` (spec §7.9): a `CONNECT` or a
/// request with an `Upgrade` field keeps the byte relay.
pub(crate) fn head_kind(buf: &[u8]) -> Head {
    match head_end(buf) {
        None => Head::Partial,
        Some(end) if leaves(&buf[..end]) => Head::Relay,
        Some(_) => Head::Routed,
    }
}

/// The length of the head: up to and including the empty line (CRLF or bare LF).
fn head_end(buf: &[u8]) -> Option<usize> {
    let mut start = 0;
    while let Some(lf) = buf[start..].iter().position(|&b| b == b'\n') {
        let at = start + lf;
        let next = &buf[at + 1..];
        if next.starts_with(b"\n") {
            return Some(at + 2);
        }
        if next.starts_with(b"\r\n") {
            return Some(at + 3);
        }
        start = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{REFUSED, Reason};

    #[test]
    fn adm_51_the_refused_head_answer_carries_the_invalid_request_reason() {
        let field = format!("\r\nx-mimic-error: {}\r\n", Reason::InvalidRequest.as_str());
        assert!(
            REFUSED.windows(field.len()).any(|w| w == field.as_bytes()),
            "REFUSED does not write Reason::InvalidRequest"
        );
    }
}
