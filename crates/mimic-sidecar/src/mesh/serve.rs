//! A mesh-routed connection (spec §7.9 "Load balancing", §4.1 "Mesh-routed HTTP"): the app's
//! connection is parsed by `hyper` and each request goes to a pod chosen by the balancer, over
//! a pooled connection. Observation is the same as on the byte relay: one access line, the RED
//! metrics and discovery's exchange per request, under the authority the app named.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, server};
use hyper_util::rt::{TokioExecutor, TokioIo};
use mimic_core::proxy::authority::status_class;
use tokio::task::JoinHandle;

use super::MeshState;
use super::metrics::Call;
use super::pool::{Lease, Protocol, Sent};
use super::router::{Backend, InFlight};
use crate::discovery::Tap;
use crate::exchange::{Ctx, RespBody, Tracker};
use crate::transport::Stream;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What a mesh-routed connection needs.
pub(crate) struct Serving {
    pub mesh: MeshState,
    pub backend: Arc<Backend>,
    pub ctx: Ctx,
    pub tap: Option<Tap>,
    /// The task relaying the tunnel an `Upgrade` or `CONNECT` request became.
    tunnel: Mutex<Option<JoinHandle<()>>>,
}

impl Serving {
    /// Ends the tunnel an upgrade request became, if one was started.
    fn abort_tunnel(&self) {
        if let Some(task) = self
            .tunnel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    pub(crate) fn new(
        mesh: MeshState,
        backend: Arc<Backend>,
        ctx: Ctx,
        tap: Option<Tap>,
    ) -> Serving {
        Serving {
            mesh,
            backend,
            ctx,
            tap,
            tunnel: Mutex::new(None),
        }
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
    _hold: Option<Hold>,
}

impl MeshBody {
    fn empty() -> MeshBody {
        MeshBody {
            upstream: None,
            _hold: None,
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
        match self.get_mut().upstream.as_mut() {
            Some(body) => Pin::new(body).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.upstream.as_ref().is_none_or(Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        self.upstream
            .as_ref()
            .map_or_else(|| SizeHint::with_exact(0), Body::size_hint)
    }
}

/// Serves `client` until it closes, `stop` completes (graceful) or `abort` completes. A
/// connection that became a tunnel is served until the tunnel ends.
pub(crate) async fn serve(
    serving: Serving,
    client: Stream,
    h2: bool,
    stop: impl Future<Output = ()> + Send,
    abort: impl Future<Output = ()> + Send,
) -> Result<(), hyper::Error> {
    let serving = Arc::new(serving);
    let protocol = if h2 { Protocol::Http2 } else { Protocol::Http1 };
    let service = {
        let serving = serving.clone();
        service_fn(move |req| forward(serving.clone(), req, protocol))
    };
    let io = TokioIo::new(client);
    let mut stop = pin!(stop);
    let mut abort = pin!(abort);
    let mut stopping = false;
    let served = if h2 {
        let connection = server::conn::http2::Builder::new(TokioExecutor::new())
            .auto_date_header(false)
            .serve_connection(io, service);
        let mut connection = pin!(connection);
        loop {
            tokio::select! {
                served = connection.as_mut() => break served,
                () = &mut stop, if !stopping => {
                    stopping = true;
                    connection.as_mut().graceful_shutdown();
                }
                () = &mut abort => {
                    serving.abort_tunnel();
                    return Ok(());
                }
            }
        }
    } else {
        let connection = server::conn::http1::Builder::new()
            .preserve_header_case(true)
            .auto_date_header(false)
            .serve_connection(io, service)
            .with_upgrades();
        let mut connection = pin!(connection);
        loop {
            tokio::select! {
                served = connection.as_mut() => break served,
                () = &mut stop, if !stopping => {
                    stopping = true;
                    connection.as_mut().graceful_shutdown();
                }
                () = &mut abort => {
                    serving.abort_tunnel();
                    return Ok(());
                }
            }
        }
    };
    let tunnel = serving
        .tunnel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(mut task) = tunnel {
        tokio::select! {
            _ = &mut task => {}
            () = &mut abort => task.abort(),
        }
    }
    served
}

async fn forward(
    serving: Arc<Serving>,
    req: Request<hyper::body::Incoming>,
    protocol: Protocol,
) -> Result<Response<MeshBody>, BoxError> {
    serving.call(req, protocol).await
}

impl Serving {
    async fn call(
        &self,
        req: Request<hyper::body::Incoming>,
        protocol: Protocol,
    ) -> Result<Response<MeshBody>, BoxError> {
        let http1 = protocol == Protocol::Http1;
        let label = if http1 { "http1" } else { "h2" };
        let connect = req.method() == Method::CONNECT;
        let upgrade = connect || (http1 && req.headers().contains_key(header::UPGRADE));
        if upgrade && !http1 {
            return Ok(answer(StatusCode::NOT_IMPLEMENTED, "unsupported", false));
        }
        let began = Instant::now();
        let mut req = req;
        // A request that asks for a tunnel keeps `Connection` and `Upgrade` (spec §4.1).
        let on_client = upgrade.then(|| hyper::upgrade::on(&mut req));
        if http1 {
            strip_hop_by_hop(req.headers_mut(), true, upgrade);
        }
        let (mut tracker, req) = Tracker::start(
            &self.ctx,
            label,
            self.tap.as_ref().filter(|_| !upgrade),
            req,
        );
        let router = self.mesh.router();
        let cluster = self.ctx.dst;
        let connect_failed = |tracker: Tracker, e: std::io::Error, pick: Option<&InFlight>| {
            tracing::debug!(error = %e, "connect failed");
            self.count(
                pick,
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
        // A stale view is not used for pod addresses (spec §7.9).
        let mut chosen: Option<(InFlight, Lease)> = None;
        if !self.mesh.stale() {
            let Some(first) = self.backend.pick(router) else {
                self.count(None, StatusCode::SERVICE_UNAVAILABLE, "no-endpoint", began);
                tracker.synthetic(StatusCode::SERVICE_UNAVAILABLE.as_u16());
                return Ok(answer(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no-endpoint",
                    false,
                ));
            };
            let first_addr = first.endpoint().addr;
            match router.pools.checkout(first_addr, protocol, false).await {
                Ok(lease) => chosen = Some((first, lease)),
                Err(e) => {
                    tracing::debug!(upstream = %first_addr, error = %e, "connect to the pod failed");
                    // Nothing was written: one other endpoint, if there is one.
                    let other = (0..4).find_map(|_| {
                        self.backend
                            .pick(router)
                            .filter(|p| p.endpoint().addr != first_addr)
                    });
                    drop(first);
                    if let Some(other) = other
                        && let Ok(lease) = router
                            .pools
                            .checkout(other.endpoint().addr, protocol, false)
                            .await
                    {
                        chosen = Some((other, lease));
                    }
                }
            }
        }
        let (pick, mut lease) = match chosen {
            Some((pick, lease)) => (Some(pick), lease),
            None => {
                // The original destination, as without the mesh.
                router.metrics.fallback(&self.backend.label);
                tracker.route(&self.backend.label, cluster);
                match router.pools.checkout(cluster, protocol, false).await {
                    Ok(lease) => (None, lease),
                    Err(e) => return connect_failed(tracker, e, None),
                }
            }
        };
        let upstream = pick.as_ref().map_or(cluster, |p| p.endpoint().addr);
        tracker.route(&self.backend.label, upstream);
        // Nothing written to a connection the pod had closed: once more on a new one, whatever
        // the method (spec §7.9).
        let mut sent = lease.send(req).await;
        if let Sent::Unsent(again) = sent {
            router.metrics.reconnect(&self.backend.label);
            lease = match router.pools.checkout(upstream, protocol, true).await {
                Ok(lease) => lease,
                Err(e) => return connect_failed(tracker, e, pick.as_ref()),
            };
            sent = lease.send(again).await;
        }
        match sent {
            Sent::Response(mut response) => {
                let tunnel = on_client.filter(|_| {
                    if connect {
                        response.status().is_success()
                    } else {
                        response.status() == StatusCode::SWITCHING_PROTOCOLS
                    }
                });
                if let Some(on_client) = tunnel {
                    let on_pod = hyper::upgrade::on(&mut response);
                    tracker.mark_upgrade();
                    let answered = tracker.answered(response);
                    self.count(pick.as_ref(), answered.status(), "none", began);
                    lease.discard();
                    let task = tokio::spawn(async move {
                        let (client, pod) = tokio::join!(on_client, on_pod);
                        if let (Ok(client), Ok(pod)) = (client, pod) {
                            let (mut client, mut pod) = (TokioIo::new(client), TokioIo::new(pod));
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut pod).await;
                        }
                        drop((pick, lease));
                    });
                    *self.tunnel.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
                    return Ok(answered.map(|body| MeshBody {
                        upstream: Some(body),
                        _hold: None,
                    }));
                }
                let mut answered = tracker.answered(response);
                if http1 {
                    strip_hop_by_hop(answered.headers_mut(), false, false);
                }
                self.count(pick.as_ref(), answered.status(), "none", began);
                Ok(answered.map(|body| MeshBody {
                    upstream: Some(body),
                    _hold: Some(Hold {
                        _in_flight: pick,
                        _lease: lease,
                    }),
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
fn strip_hop_by_hop(headers: &mut HeaderMap, request: bool, upgrade: bool) {
    if upgrade {
        headers.remove("keep-alive");
        headers.remove("proxy-connection");
        return;
    }
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
    let Some(end) = head_end(buf) else {
        return Head::Partial;
    };
    let head = &buf[..end];
    let mut lines = head.split(|&b| b == b'\n');
    let request_line = lines.next().unwrap_or_default();
    if request_line
        .get(..8)
        .is_some_and(|m| m.eq_ignore_ascii_case(b"CONNECT "))
    {
        return Head::Relay;
    }
    for line in lines {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        if line[..colon].trim_ascii().eq_ignore_ascii_case(b"upgrade") {
            return Head::Relay;
        }
    }
    Head::Routed
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
