//! A mesh-routed connection (spec §7.9 "Load balancing", §4.1 "Mesh-routed HTTP"): the app's
//! connection is parsed by `hyper` and each request goes to a pod chosen by the balancer, over
//! a pooled connection. Observation is the same as on the byte relay: one access line, the RED
//! metrics and discovery's exchange per request, under the authority the app named. On HTTP/1
//! a head reader ([`super::gate`]) judges each request head before `hyper` sees it.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::mesh::head::{Stop, leaves};
use mimic_core::proxy::authority::status_class;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::MeshState;
use super::dial::CLUSTER_CONNECT_TIMEOUT;
use super::gate::{Gate, Progress};
use super::metrics::Call;
use super::pool::{Lease, Protocol, Sent};
use super::router::{Backend, InFlight};
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
/// is a warning with the TLS error, any other failure is routine.
pub(super) fn connect_failed_log(upstream: &SocketAddr, e: &io::Error) {
    if is_tls_error(e) {
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

/// A connection for one request: the pod it goes to (counted in flight) or the original
/// destination (`pick` none).
struct Conn {
    pick: Option<InFlight>,
    lease: Lease,
    upstream: SocketAddr,
}

/// Why no connection could be had.
enum Unreachable {
    NoEndpoint,
    Connect(io::Error),
}

impl Serving {
    /// A connection for a request: the pod P2C chooses, one other pod when connecting to it
    /// fails, then the original destination (spec §7.9). A stale view or a view that says
    /// `"mesh": false` goes to the original destination at once.
    async fn connect(&self, protocol: Protocol) -> Result<Conn, Unreachable> {
        let router = self.mesh.router();
        if !self.mesh.stale() && !self.mesh.off() {
            let mut failed = None;
            for _ in 0..2 {
                let pick = match failed {
                    None => self.backend.pick(router),
                    Some(addr) => self.backend.pick_other(router, addr),
                };
                let Some(pick) = pick else {
                    if failed.is_none() {
                        return Err(Unreachable::NoEndpoint);
                    }
                    break;
                };
                let upstream = pick.endpoint().addr;
                match router
                    .pools
                    .checkout(
                        upstream,
                        protocol,
                        self.origin.as_ref(),
                        None,
                        router.connect_timeout,
                    )
                    .await
                {
                    Ok(lease) => {
                        router.succeeded(&pick);
                        return Ok(Conn {
                            pick: Some(pick),
                            lease,
                            upstream,
                        });
                    }
                    Err(e) => {
                        connect_failed_log(&upstream, &e);
                        router.failed(&pick);
                        failed = Some(upstream);
                    }
                }
            }
        }
        self.fallback(protocol).await
    }

    /// The original destination, pooled like a pod, so that kube-proxy chooses as it does
    /// without the mesh; counted as a fallback.
    async fn fallback(&self, protocol: Protocol) -> Result<Conn, Unreachable> {
        let router = self.mesh.router();
        let upstream = self.ctx.dst;
        router.metrics.fallback(&self.backend.label);
        router
            .pools
            .checkout(
                upstream,
                protocol,
                self.origin.as_ref(),
                None,
                CLUSTER_CONNECT_TIMEOUT,
            )
            .await
            .map(|lease| Conn {
                pick: None,
                lease,
                upstream,
            })
            .map_err(|e| {
                connect_failed_log(&upstream, &e);
                Unreachable::Connect(e)
            })
    }

    /// A new connection after nothing could be written to `failed`: to the same pod, never the
    /// connection that failed, and when that cannot be made either (nothing was written yet) to
    /// the original destination.
    async fn reconnect(&self, protocol: Protocol, failed: Conn) -> Result<Conn, Unreachable> {
        let router = self.mesh.router();
        let Conn {
            pick,
            lease,
            upstream,
        } = failed;
        let timeout = if pick.is_some() {
            router.connect_timeout
        } else {
            CLUSTER_CONNECT_TIMEOUT
        };
        match router
            .pools
            .checkout(
                upstream,
                protocol,
                self.origin.as_ref(),
                Some(&lease),
                timeout,
            )
            .await
        {
            Ok(lease) => Ok(Conn {
                pick,
                lease,
                upstream,
            }),
            Err(e) => {
                connect_failed_log(&upstream, &e);
                match pick {
                    Some(pick) => {
                        router.failed(&pick);
                        drop(pick);
                        self.fallback(protocol).await
                    }
                    None => Err(Unreachable::Connect(e)),
                }
            }
        }
    }

    /// A head the reader refused: the access line and the count (the `400` itself is written
    /// by the caller).
    fn refused(&self, request: Option<&mimic_core::proxy::http1::RequestHead>) {
        exchange::refused(&self.ctx, request);
        self.count(
            None,
            StatusCode::BAD_REQUEST,
            "invalid-request",
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
            let (tracker, _) = Tracker::start(&self.ctx, label, None, req);
            self.count(None, StatusCode::NOT_IMPLEMENTED, "unsupported", began);
            tracker.synthetic(StatusCode::NOT_IMPLEMENTED.as_u16());
            return Ok(answer(StatusCode::NOT_IMPLEMENTED, "unsupported", http1));
        }
        let mut req = req;
        if http1 {
            strip_hop_by_hop(req.headers_mut(), true);
        }
        let (mut tracker, req) = Tracker::start(&self.ctx, label, self.tap.as_ref(), req);
        let router = self.mesh.router();
        let connect_failed = |tracker: Tracker, e: &dyn std::fmt::Display| {
            tracing::debug!(error = %e, "connect failed");
            self.count(
                None,
                StatusCode::SERVICE_UNAVAILABLE,
                "connect-failed",
                began,
            );
            tracker.synthetic(StatusCode::SERVICE_UNAVAILABLE.as_u16());
            Ok(answer(
                StatusCode::SERVICE_UNAVAILABLE,
                "connect-failed",
                false,
            ))
        };
        let mut conn = match self.connect(protocol).await {
            Ok(conn) => conn,
            Err(Unreachable::NoEndpoint) => {
                self.count(None, StatusCode::SERVICE_UNAVAILABLE, "no-endpoint", began);
                tracker.synthetic(StatusCode::SERVICE_UNAVAILABLE.as_u16());
                return Ok(answer(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no-endpoint",
                    false,
                ));
            }
            Err(Unreachable::Connect(e)) => return connect_failed(tracker, &e),
        };
        tracker.route(&self.backend.label, conn.upstream);
        // Nothing written to a connection the pod had closed: once more on a new one, whatever
        // the method (spec §7.9).
        let mut sent = conn.lease.send(req).await;
        if let Sent::Unsent(again) = sent {
            router.metrics.reconnect(&self.backend.label);
            conn = match self.reconnect(protocol, conn).await {
                Ok(conn) => conn,
                Err(Unreachable::Connect(e)) => return connect_failed(tracker, &e),
                Err(Unreachable::NoEndpoint) => return connect_failed(tracker, &"no endpoint"),
            };
            tracker.route(&self.backend.label, conn.upstream);
            sent = conn.lease.send(again).await;
        }
        let Conn {
            pick,
            lease,
            upstream,
        } = conn;
        match sent {
            Sent::Response(response) => {
                let mut answered = tracker.answered(response);
                if http1 {
                    strip_hop_by_hop(answered.headers_mut(), false);
                }
                self.count(pick.as_ref(), answered.status(), "none", began);
                Ok(answered.map(|body| MeshBody {
                    upstream: Some(body),
                    mocked: None,
                    _hold: Some(Hold {
                        _in_flight: pick,
                        _lease: lease,
                    }),
                    done: None,
                }))
            }
            Sent::Failed(e) if e.is_parse() || e.is_parse_status() || e.is_parse_too_large() => {
                tracing::debug!(%upstream, error = %e, "the pod's response head was refused");
                self.count(
                    pick.as_ref(),
                    StatusCode::BAD_GATEWAY,
                    "invalid-response",
                    began,
                );
                tracker.synthetic(StatusCode::BAD_GATEWAY.as_u16());
                Ok(answer(StatusCode::BAD_GATEWAY, "invalid-response", false))
            }
            Sent::Failed(e) => {
                tracing::debug!(%upstream, error = %e, "the pod did not answer");
                self.count_none(pick.as_ref(), began);
                tracker.failed();
                Err(e.into())
            }
            Sent::Unsent(_) => {
                tracing::debug!(%upstream, "the request could not be written twice");
                self.count_none(pick.as_ref(), began);
                tracker.failed();
                Err("connection closed".into())
            }
        }
    }

    fn count(
        &self,
        pick: Option<&InFlight>,
        status: StatusCode,
        reason: &'static str,
        began: Instant,
    ) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                backend: &self.backend.label,
                endpoint: pick.map(|p| &*p.endpoint().label),
                status_class: status_class(status.as_u16()),
                reason,
            },
            began.elapsed(),
        );
    }

    /// A request that got no response.
    fn count_none(&self, pick: Option<&InFlight>, began: Instant) {
        self.mesh.router().metrics.request(
            &Call {
                service: &self.backend.service,
                backend: &self.backend.label,
                endpoint: pick.map(|p| &*p.endpoint().label),
                status_class: "none",
                reason: "none",
            },
            began.elapsed(),
        );
    }
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
