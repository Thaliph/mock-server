//! A mesh-routed connection (spec §7.9 "Load balancing", §4.1 "Mesh-routed HTTP"): the app's
//! connection is parsed by `hyper` and each request goes to a pod chosen by the balancer, over
//! a pooled connection. Observation is the same as on the byte relay: one access line, the RED
//! metrics and discovery's exchange per request, under the authority the app named. On HTTP/1
//! a head reader ([`super::gate`]) judges each request head before `hyper` sees it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::head::{Stop, leaves};
use mimic_core::mesh::retry::{
    Decision, Failure, Outcome, Planner, Refusal, Verdict, classify, delay, trusted,
};
use mimic_core::proxy::authority::status_class;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::MeshState;
use super::dial::CLUSTER_CONNECT_TIMEOUT;
use super::gate::{Gate, Progress};
use super::metrics::Call;
use super::pool::{self, Lease, Protocol, Sent};
use super::replay::Source;
use super::router::{Backend, InFlight, Plan};
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
    _lease: Lease,
}

/// The body of a response: the pod's, or none for an answer of the sidecar.
pub(crate) struct MeshBody {
    upstream: Option<RespBody>,
    mocked: Option<UnsyncBoxBody<Bytes, hyper::Error>>,
    _hold: Option<Hold>,
    done: Option<Done>,
}

impl MeshBody {
    fn empty() -> MeshBody {
        MeshBody {
            upstream: None,
            mocked: None,
            _hold: None,
            done: None,
        }
    }
}

impl Body for MeshBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = self.get_mut();
        match (this.upstream.as_mut(), this.mocked.as_mut()) {
            (Some(body), _) => Pin::new(body).poll_frame(cx),
            (None, Some(body)) => Pin::new(body).poll_frame(cx),
            (None, None) => Poll::Ready(None),
        }
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
        response: Response<hyper::body::Incoming>,
        conn: Conn,
    },
    /// No response head. `error` is the failure of a request that was written.
    Failed {
        failure: Failure,
        error: Option<hyper::Error>,
        pick: Option<InFlight>,
        written: bool,
    },
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
        source: &Source,
        head: &Parts,
        warned: &AtomicBool,
    ) -> Tried {
        let router = self.mesh.router();
        let upstream = pick.as_ref().map_or(self.ctx.dst, |p| p.endpoint().addr);
        let timeout = if pick.is_some() {
            router.connect_timeout
        } else {
            CLUSTER_CONNECT_TIMEOUT
        };
        let connected = router
            .pools
            .checkout(upstream, protocol, self.origin.as_ref(), None, timeout)
            .await;
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
        let mut sent = lease.send(source.request(head)).await;
        if let Sent::Unsent = sent {
            if !source.intact() {
                return Tried::Failed {
                    failure: Failure::Reset,
                    error: None,
                    pick,
                    written: true,
                };
            }
            router.metrics.reconnect(&self.backend.label);
            let again = router
                .pools
                .checkout(
                    upstream,
                    protocol,
                    self.origin.as_ref(),
                    Some(&lease),
                    timeout,
                )
                .await;
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
            sent = lease.send(source.request(head)).await;
        }
        match sent {
            Sent::Response(response) => Tried::Answer {
                response,
                conn: Conn { pick, lease },
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

    /// The next try after one failed: `Stop` without one left. A stale view, or one that says
    /// `"mesh": false`, has the original destination for its only try (spec §7.9).
    fn plan(&self, direct: bool, tries: u32, planner: &mut Planner) -> Plan {
        if direct {
            return if tries == 0 {
                Plan::Original
            } else {
                Plan::Stop
            };
        }
        self.backend.plan(self.mesh.router(), planner)
    }

    /// What to do with a failed try: a stale view, or one that says `"mesh": false`, has no
    /// try after the original destination and draws nothing on the budget.
    fn decide(&self, direct: bool, verdict: Verdict, planner: &Planner) -> Decision {
        if direct && verdict.retry {
            return Decision::Exhausted;
        }
        self.backend
            .retry
            .decide(verdict, planner, self.mesh.router().now())
    }

    /// Counts a failure that is not retried (`Retry` is handled by the caller).
    fn not_retried(&self, decision: Decision) {
        let router = self.mesh.router();
        let backend = &self.backend.label;
        match decision {
            Decision::Skipped(skip) => router.metrics.skipped(backend, skip.label()),
            Decision::Exhausted => router.metrics.skipped(backend, "exhausted"),
            Decision::Budget => self.backend.exhausted(router),
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
        self.count(StatusCode::BAD_REQUEST, "invalid-request", Instant::now());
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
                _hold: None,
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
            self.count(StatusCode::NOT_IMPLEMENTED, "unsupported", began);
            tracker.synthetic(StatusCode::NOT_IMPLEMENTED.as_u16());
            return Ok(answer(StatusCode::NOT_IMPLEMENTED, "unsupported", http1));
        }
        let mut req = req;
        if http1 {
            strip_hop_by_hop(req.headers_mut(), true);
        }
        let (mut tracker, req) = Tracker::start(&self.ctx, label, self.tap.as_ref(), req);
        let router = self.mesh.router();
        let backend = &self.backend.label;
        let (head, body) = req.into_parts();
        let source = Source::new(body, self.mesh.replay());
        let direct = self.mesh.stale() || self.mesh.off();
        let mut planner = Planner::new(self.backend.retry.retries());
        self.backend.retry.deposit(router.now());
        let warned = AtomicBool::new(false);
        let mut tries = 0;
        let mut again: Option<Outcome> = None;
        let mut next = self.plan(direct, tries, &mut planner);
        loop {
            let pick = match next {
                Plan::Pod(pick) => Some(pick),
                Plan::Original => None,
                Plan::Stop => {
                    self.count(StatusCode::SERVICE_UNAVAILABLE, "no-endpoint", began);
                    tracker.synthetic(StatusCode::SERVICE_UNAVAILABLE.as_u16());
                    return Ok(answer(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "no-endpoint",
                        false,
                    ));
                }
            };
            tries += 1;
            if let Some(reason) = again.take() {
                router.metrics.retry(backend, reason);
            }
            if pick.is_none() {
                router.metrics.fallback(backend);
            }
            let upstream = pick.as_ref().map_or(self.ctx.dst, |p| p.endpoint().addr);
            tracker.route(backend, upstream, tries);
            match self.try_once(protocol, pick, &source, &head, &warned).await {
                Tried::Answer { response, mut conn } => {
                    let status = response.status();
                    let refusal = refusal_of(&response, conn.pick.as_ref(), self.origin.is_some());
                    let failure = refusal.map_or(Failure::Response, Failure::Refusal);
                    let verdict = classify(failure, head.method.as_str(), source.held(), true);
                    router.metrics.attempt(backend, verdict.outcome);
                    if let Some(pick) = &conn.pick {
                        let class = status_class(status.as_u16());
                        router
                            .metrics
                            .endpoint_try(backend, &pick.endpoint().label, class);
                    }
                    planner.failed(verdict.outcome);
                    match self.decide(direct, verdict, &planner) {
                        Decision::Retry => match self.plan(direct, tries, &mut planner) {
                            Plan::Stop => router.metrics.skipped(backend, "exhausted"),
                            // The try in flight stops reading the app's body now, so that
                            // what the next try replays is what was read.
                            retry if source.detach() => {
                                Self::discard(response, &mut conn.lease).await;
                                drop(conn);
                                self.pause(verdict.outcome).await;
                                again = Some(verdict.outcome);
                                next = retry;
                                continue;
                            }
                            _ => router.metrics.skipped(backend, "buffer"),
                        },
                        other => self.not_retried(other),
                    }
                    source.release();
                    let Conn { pick, lease } = conn;
                    let answered = match refusal {
                        Some(_) => tracker.answered_unrecorded(response),
                        None => tracker.answered(response),
                    };
                    let mut answered = answered;
                    if http1 {
                        strip_hop_by_hop(answered.headers_mut(), false);
                    }
                    let reason = refusal.map_or("none", |r| match r {
                        Refusal::Overloaded => "overloaded",
                        Refusal::QueueTimeout => "queue-timeout",
                        Refusal::AppRefused => "app-refused",
                    });
                    self.count(answered.status(), reason, began);
                    return Ok(answered.map(|body| MeshBody {
                        upstream: Some(body),
                        mocked: None,
                        _hold: Some(Hold {
                            _in_flight: pick,
                            _lease: lease,
                        }),
                        done: None,
                    }));
                }
                Tried::Failed {
                    failure,
                    error,
                    pick,
                    written,
                } => {
                    let verdict = classify(failure, head.method.as_str(), source.held(), written);
                    router.metrics.attempt(backend, verdict.outcome);
                    if let (true, Some(pick)) = (written, &pick) {
                        let label = &pick.endpoint().label;
                        router.metrics.endpoint_try(backend, label, "none");
                    }
                    planner.failed(verdict.outcome);
                    match self.decide(direct, verdict, &planner) {
                        Decision::Retry => match self.plan(direct, tries, &mut planner) {
                            Plan::Stop => router.metrics.skipped(backend, "exhausted"),
                            retry if source.detach() => {
                                drop(pick);
                                again = Some(verdict.outcome);
                                next = retry;
                                continue;
                            }
                            _ => router.metrics.skipped(backend, "buffer"),
                        },
                        other => self.not_retried(other),
                    }
                    return self.failed(tracker, failure, error, upstream, began);
                }
            }
        }
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
    /// the app's connection or stream.
    fn failed(
        &self,
        tracker: Tracker,
        failure: Failure,
        error: Option<hyper::Error>,
        upstream: SocketAddr,
        began: Instant,
    ) -> Result<Response<MeshBody>, BoxError> {
        match failure {
            Failure::Connect => {
                tracing::debug!(%upstream, "connect failed");
                self.count(StatusCode::SERVICE_UNAVAILABLE, "connect-failed", began);
                tracker.synthetic(StatusCode::SERVICE_UNAVAILABLE.as_u16());
                Ok(answer(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "connect-failed",
                    false,
                ))
            }
            Failure::Response => {
                tracing::debug!(%upstream, "the pod's response head was refused");
                self.count(StatusCode::BAD_GATEWAY, "invalid-response", began);
                tracker.synthetic(StatusCode::BAD_GATEWAY.as_u16());
                Ok(answer(StatusCode::BAD_GATEWAY, "invalid-response", false))
            }
            _ => {
                tracing::debug!(%upstream, "the pod did not answer");
                self.count_none(began);
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

    fn count(&self, status: StatusCode, reason: &'static str, began: Instant) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                backend: &self.backend.label,
                status_class: status_class(status.as_u16()),
                reason,
            },
            began.elapsed(),
        );
    }

    /// A request that got no response.
    fn count_none(&self, began: Instant) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                backend: &self.backend.label,
                status_class: "none",
                reason: "none",
            },
            began.elapsed(),
        );
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

/// An answer of the sidecar: `x-mimic-error` and no body. With `close` the connection ends.
fn answer(status: StatusCode, reason: &'static str, close: bool) -> Response<MeshBody> {
    let mut response = Response::new(MeshBody::empty());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("x-mimic-error", HeaderValue::from_static(reason));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
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
