//! Observation of HTTP exchanges (spec §4.1, §9). HTTP/1: an [`Http1Tracker`] fed with the
//! events of the codec writes the access line and the RED metrics of each exchange. HTTP/2:
//! body wrappers that count the bytes relayed and a [`Tracker`] that does the same; the
//! wrappers forward every frame, size hint and end-of-stream signal unchanged, so framing is
//! not altered.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap};
use hyper::{Request, Response};
use mimic_core::proxy::authority::{normalise_authority, target_authority};
use mimic_core::proxy::http1::{CAPTURE_BODY_LIMIT, Captured, Event, RequestHead, Side};

use crate::access::{self, Outcome};
use crate::discovery::{Capture, Meta, Tap};
use crate::listener::Direction;
use crate::red::ProxyMetrics;

/// Default port of a `Host` header on plain HTTP, removed by the authority normalisation.
const HTTP_PORT: u16 = 80;
/// The same over TLS (intercepted connections).
const HTTPS_PORT: u16 = 443;

const PROTOCOL_H2: &str = "h2";

/// What the observation of a connection needs to know about it.
#[derive(Clone)]
pub struct Ctx {
    pub direction: Direction,
    pub dst: SocketAddr,
    /// The exchanges are decrypted by interception (spec §5).
    pub tls: bool,
    pub proxy: ProxyMetrics,
    /// True once the connections left at the end of the shutdown grace are being closed.
    pub aborted: tokio::sync::watch::Receiver<bool>,
}

/// Request body bytes seen so far, and whether the client side of the body failed.
#[derive(Default)]
pub(crate) struct RequestCount {
    bytes: AtomicU64,
    failed: AtomicBool,
}

/// The request body, counted.
pub(crate) struct ReqBody {
    inner: Incoming,
    /// The request carried a `content-length`: only then is the length known to hand on.
    sized: bool,
    count: Arc<RequestCount>,
    /// Copies the body for discovery (spec §6).
    tee: Option<Tee>,
}

/// The exchange being captured, shared by the request side, the response side and the tracker.
type SharedCapture = Arc<Mutex<Option<Capture>>>;

/// Copies the body frames that pass into the [`Capture`] of the exchange, up to the capture
/// limit; the frames themselves are forwarded unchanged.
struct Tee {
    capture: SharedCapture,
    request: bool,
    kept: usize,
    ended: bool,
}

impl Tee {
    fn new(capture: &SharedCapture, request: bool) -> Tee {
        Tee {
            capture: capture.clone(),
            request,
            kept: 0,
            ended: false,
        }
    }

    fn with(&self, f: impl FnOnce(&mut Capture)) {
        let mut capture = self.capture.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(capture) = capture.as_mut() {
            f(capture);
        }
    }

    fn data(&mut self, data: &[u8]) {
        let take = data.len().min(CAPTURE_BODY_LIMIT - self.kept);
        if take == 0 {
            return;
        }
        self.kept += take;
        let bytes = data[..take].to_vec();
        let captured = if self.request {
            Captured::RequestBody(bytes)
        } else {
            Captured::ResponseBody(bytes)
        };
        self.with(|capture| capture.push(captured));
    }

    /// The body is complete: `total` payload bytes went through.
    fn end(&mut self, total: u64) {
        if self.ended {
            return;
        }
        self.ended = true;
        let request = self.request;
        self.with(|capture| {
            if request {
                capture.request_end(total);
            } else {
                capture.response_end(total);
            }
        });
    }
}

impl Body for ReqBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.count
                        .bytes
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    if let Some(tee) = this.tee.as_mut() {
                        tee.data(data);
                    }
                }
                // The server may not poll again once it knows the body is complete.
                if this.inner.is_end_stream()
                    && let Some(tee) = this.tee.as_mut()
                {
                    tee.end(this.count.bytes.load(Ordering::Relaxed));
                }
            }
            Poll::Ready(None) => {
                if let Some(tee) = this.tee.as_mut() {
                    tee.end(this.count.bytes.load(Ordering::Relaxed));
                }
            }
            Poll::Ready(Some(Err(_))) => this.count.failed.store(true, Ordering::Relaxed),
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        // `hyper` reports an exact zero for a body-less HTTP/2 request, and its client turns
        // that into a `content-length: 0` the client never sent.
        if self.sized {
            self.inner.size_hint()
        } else {
            SizeHint::default()
        }
    }
}

/// One exchange being relayed. It writes its access line and counts its bytes when it ends.
pub(crate) struct Tracker {
    ctx: Ctx,
    protocol: &'static str,
    started: Instant,
    authority: String,
    method: String,
    path: String,
    request: Arc<RequestCount>,
    status: Option<u16>,
    head: Option<Duration>,
    grpc_status: Option<u64>,
    /// The request was counted in the RED metrics.
    counted: bool,
    /// The access line was written.
    ended: bool,
    /// The bytes discovery captures (spec §6), when it is on for this connection.
    capture: Option<SharedCapture>,
    tap: Option<Tap>,
    /// The authority the request named, normalised.
    named_authority: Option<String>,
}

/// A head as the HTTP/1 text the recorder parses: the HTTP/2 message has none.
fn head_text(first_line: &str, headers: &HeaderMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(first_line.len() + 64 * headers.len());
    out.extend_from_slice(first_line.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn grpc_status(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

impl Ctx {
    /// The default port of an authority on this connection.
    fn default_port(&self) -> u16 {
        if self.tls { HTTPS_PORT } else { HTTP_PORT }
    }
}

impl Tracker {
    /// Starts the observation of `req` and wraps its body.
    pub(crate) fn start(
        ctx: &Ctx,
        protocol: &'static str,
        tap: Option<&Tap>,
        req: Request<Incoming>,
    ) -> (Tracker, Request<ReqBody>) {
        let host = req
            .headers()
            .get(header::HOST)
            .map(|host| host.to_str().unwrap_or(""));
        let named_authority = target_authority(req.uri().authority().map(|a| a.as_str()), host)
            .map(|raw| normalise_authority(raw, ctx.default_port()));
        let authority = named_authority
            .clone()
            .unwrap_or_else(|| ctx.dst.to_string());
        let request = Arc::new(RequestCount::default());
        // An extended `CONNECT` is a tunnel, not an exchange.
        let capture = tap
            .filter(|_| req.method() != hyper::Method::CONNECT)
            .map(|tap| {
                let mut capture = tap.capture();
                let target = req
                    .uri()
                    .path_and_query()
                    .map_or("/", |p| p.as_str())
                    .to_string();
                capture.push(Captured::RequestHead(head_text(
                    &format!("{} {target} HTTP/1.1", req.method()),
                    req.headers(),
                )));
                Arc::new(Mutex::new(Some(capture)))
            });
        let tracker = Tracker {
            ctx: ctx.clone(),
            protocol,
            started: Instant::now(),
            authority,
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            request: request.clone(),
            status: None,
            head: None,
            grpc_status: None,
            counted: false,
            ended: false,
            capture: capture.clone(),
            tap: tap.cloned(),
            named_authority,
        };
        let (parts, inner) = req.into_parts();
        let mut tee = capture.as_ref().map(|c| Tee::new(c, true));
        if let Some(tee) = tee.as_mut()
            && inner.is_end_stream()
        {
            tee.end(0);
        }
        let body = ReqBody {
            inner,
            sized: parts.headers.contains_key(header::CONTENT_LENGTH),
            count: request,
            tee,
        };
        (tracker, Request::from_parts(parts, body))
    }

    /// The destination did not answer: the exchange ends here.
    pub(crate) fn failed(mut self) {
        self.count_unanswered();
        self.finish(Outcome::UpstreamReset, 0);
    }

    /// Counts a request that got no response (`status_class="none"`).
    fn count_unanswered(&mut self) {
        self.counted = true;
        self.ctx.proxy.request(
            self.ctx.direction,
            self.protocol,
            &self.authority,
            None,
            self.started.elapsed(),
        );
    }

    /// The response head arrived: counts the request and wraps the response body, which ends
    /// the exchange.
    pub(crate) fn answered(mut self, response: Response<Incoming>) -> Response<RespBody> {
        let head = self.started.elapsed();
        let status = response.status().as_u16();
        self.status = Some(status);
        self.head = Some(head);
        self.grpc_status = grpc_status(response.headers());
        self.counted = true;
        self.ctx.proxy.request(
            self.ctx.direction,
            self.protocol,
            &self.authority,
            Some(status),
            head,
        );
        let (parts, inner) = response.into_parts();
        let mut tee = self.capture.as_ref().map(|c| Tee::new(c, false));
        if let (Some(tee), Some(capture)) = (tee.as_mut(), self.capture.as_ref()) {
            let line = format!("HTTP/1.1 {status}");
            if let Some(capture) = capture
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                capture.push(Captured::ResponseHead(head_text(&line, &parts.headers)));
            }
            if inner.is_end_stream() {
                tee.end(0);
            }
        }
        let body = RespBody {
            inner,
            tracker: Some(self),
            bytes_out: 0,
            trailers_seen: false,
            tee,
        };
        Response::from_parts(parts, body)
    }

    /// Hands an exchange that got a response head to discovery.
    fn discover(&mut self) {
        let (Some(capture), Some(tap), Some(status), Some(head)) = (
            self.capture
                .take()
                .and_then(|c| c.lock().unwrap_or_else(PoisonError::into_inner).take()),
            self.tap.as_ref(),
            self.status,
            self.head,
        ) else {
            return;
        };
        tap.submit(
            capture,
            Meta {
                port: self.ctx.dst.port(),
                authority: self.named_authority.take(),
                status,
                head_ms: head.as_millis() as u64,
                tunnel: false,
            },
        );
    }

    fn finish(&mut self, outcome: Outcome, bytes_out: u64) {
        self.ended = true;
        self.discover();
        let failed = self.request.failed.load(Ordering::Relaxed);
        let outcome = if outcome == Outcome::Ok {
            outcome
        } else if *self.ctx.aborted.borrow() {
            Outcome::Shutdown
        } else if failed {
            Outcome::ClientAbort
        } else {
            outcome
        };
        let bytes_in = self.request.bytes.load(Ordering::Relaxed);
        self.ctx
            .proxy
            .bytes(self.ctx.direction, self.protocol, bytes_in, bytes_out);
        access::exchange(&access::Exchange {
            direction: self.ctx.direction,
            protocol: self.protocol,
            dst: self.ctx.dst,
            authority: std::mem::take(&mut self.authority),
            method: Some(std::mem::take(&mut self.method)),
            path: Some(std::mem::take(&mut self.path)),
            status: self.status,
            grpc_status: self.grpc_status,
            upgrade: false,
            tls: self.ctx.tls,
            outcome,
            duration: Some(self.started.elapsed()),
            head: self.head,
            bytes_in,
            bytes_out,
        });
    }
}

impl Drop for Tracker {
    /// The service future was dropped before the exchange ended (the client left, or the
    /// connection was torn down): the client aborted.
    fn drop(&mut self) {
        if !self.ended {
            if !self.counted {
                self.count_unanswered();
            }
            self.finish(Outcome::ClientAbort, 0);
        }
    }
}

/// The response body, counted. Reaching its end, failing or being dropped ends the exchange.
pub(crate) struct RespBody {
    inner: Incoming,
    tracker: Option<Tracker>,
    bytes_out: u64,
    /// The trailers, the last frame, were handed over: the server may not poll for the end.
    trailers_seen: bool,
    /// Copies the body for discovery (spec §6).
    tee: Option<Tee>,
}

impl RespBody {
    fn end(&mut self, outcome: Outcome) {
        if self.inner.is_end_stream()
            && let Some(tee) = self.tee.as_mut()
        {
            tee.end(self.bytes_out);
        }
        if let Some(mut tracker) = self.tracker.take() {
            tracker.finish(outcome, self.bytes_out);
        }
    }
}

impl Body for RespBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes_out += data.len() as u64;
                    if let Some(tee) = this.tee.as_mut() {
                        tee.data(data);
                    }
                } else if let Some(trailers) = frame.trailers_ref() {
                    this.trailers_seen = true;
                    if let (Some(tracker), Some(status)) =
                        (this.tracker.as_mut(), grpc_status(trailers))
                    {
                        tracker.grpc_status = Some(status);
                    }
                }
            }
            Poll::Ready(None) => {
                if let Some(tee) = this.tee.as_mut() {
                    tee.end(this.bytes_out);
                }
                this.end(Outcome::Ok);
            }
            Poll::Ready(Some(Err(_))) => this.end(Outcome::UpstreamReset),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for RespBody {
    fn drop(&mut self) {
        // The server stops polling a body whose length it knows once it has read it all.
        let outcome = if self.trailers_seen || self.inner.is_end_stream() {
            Outcome::Ok
        } else {
            Outcome::ClientAbort
        };
        self.end(outcome);
    }
}

/// An HTTP/2 client connection broken at connection level (the sidecar sent `GOAWAY`): no
/// request is known, no response head was read, so nothing is timed.
pub(crate) fn rejected(ctx: &Ctx) {
    let authority = ctx.dst.to_string();
    ctx.proxy
        .request_unobserved(ctx.direction, PROTOCOL_H2, &authority);
    access::exchange(&access::Exchange {
        direction: ctx.direction,
        protocol: PROTOCOL_H2,
        dst: ctx.dst,
        authority,
        method: None,
        path: None,
        status: None,
        grpc_status: None,
        upgrade: false,
        tls: ctx.tls,
        outcome: Outcome::Rejected,
        duration: None,
        head: None,
        bytes_in: 0,
        bytes_out: 0,
    });
}

const PROTOCOL_H1: &str = "http1";

/// An HTTP/1 exchange seen by the codec and not written yet.
struct Open {
    started: Instant,
    /// `None` for a response that came while no request was pending.
    request: Option<RequestHead>,
    request_done: bool,
    bytes_in: u64,
    /// Status of the final response, once its head is read.
    status: Option<u16>,
    head: Option<Duration>,
    bytes_out: u64,
    response_done: bool,
    /// The exchange became a tunnel (`101`, `CONNECT` accepted).
    tunnel: bool,
    /// The request was counted in the RED metrics (with its status).
    counted: bool,
    /// The bytes discovery captures (spec §6), when it is on for this connection.
    capture: Option<Capture>,
}

impl Open {
    fn new(request: Option<RequestHead>) -> Open {
        Open {
            started: Instant::now(),
            request_done: request.is_none(),
            request,
            bytes_in: 0,
            status: None,
            head: None,
            bytes_out: 0,
            response_done: false,
            tunnel: false,
            counted: false,
            capture: None,
        }
    }
}

/// Observation of the HTTP/1 exchanges of one connection, fed with the events of the codec.
/// Every exchange writes its access line and its RED metrics when it ends: the response is
/// complete, the codec gave up (`unframed`) or the relay ended.
pub(crate) struct Http1Tracker {
    ctx: Ctx,
    open: VecDeque<Open>,
    /// Where the exchanges go for discovery.
    tap: Option<Tap>,
}

impl Http1Tracker {
    pub(crate) fn new(ctx: Ctx) -> Http1Tracker {
        Http1Tracker {
            ctx,
            open: VecDeque::new(),
            tap: None,
        }
    }

    /// Hands the exchanges to discovery as well.
    pub(crate) fn with_tap(mut self, tap: Option<Tap>) -> Http1Tracker {
        self.tap = tap;
        self
    }

    fn authority(&self, request: Option<&RequestHead>) -> String {
        request
            .and_then(|r| target_authority(r.uri_authority.as_deref(), r.host.as_deref()))
            .map_or_else(
                || self.ctx.dst.to_string(),
                |raw| normalise_authority(raw, self.ctx.default_port()),
            )
    }

    pub(crate) fn event(&mut self, event: Event) {
        match event {
            Event::Request(head) => {
                let capture = self
                    .tap
                    .as_ref()
                    .filter(|_| !head.connect)
                    .map(Tap::capture);
                let mut open = Open::new(Some(head));
                open.capture = capture;
                self.open.push_back(open);
            }
            Event::Captured(captured) => self.captured(captured),
            Event::RequestEnd { body_bytes } => {
                if let Some(open) = self.open.iter_mut().rev().find(|o| !o.request_done) {
                    open.request_done = true;
                    open.bytes_in = body_bytes;
                    if let Some(capture) = open.capture.as_mut() {
                        capture.request_end(body_bytes);
                    }
                }
                self.reap();
            }
            Event::Interim(_) => {}
            Event::Response(head) => {
                let index = match self.open.iter().position(|o| o.status.is_none()) {
                    Some(index) => index,
                    None => {
                        self.open.push_back(Open::new(None));
                        self.open.len() - 1
                    }
                };
                let authority = self.authority(self.open[index].request.as_ref());
                let open = &mut self.open[index];
                let elapsed = open.started.elapsed();
                open.status = Some(head.status);
                open.head = Some(elapsed);
                open.counted = true;
                self.ctx.proxy.request(
                    self.ctx.direction,
                    PROTOCOL_H1,
                    &authority,
                    Some(head.status),
                    elapsed,
                );
            }
            Event::ResponseEnd { body_bytes } => {
                if let Some(open) = self
                    .open
                    .iter_mut()
                    .find(|o| o.status.is_some() && !o.response_done)
                {
                    open.response_done = true;
                    open.bytes_out = body_bytes;
                    if let Some(capture) = open.capture.as_mut() {
                        capture.response_end(body_bytes);
                    }
                }
                self.reap();
            }
            Event::Tunnel => {
                // The exchange ends with its response head: the rest is not HTTP.
                if let Some(open) = self
                    .open
                    .iter_mut()
                    .find(|o| o.status.is_some() && !o.response_done)
                {
                    open.tunnel = true;
                    open.response_done = true;
                }
                self.reap();
            }
            Event::Unframed(unframed) => {
                let in_queue = match unframed.side {
                    Side::Client => self.open.back().is_some_and(|o| !o.request_done),
                    Side::Server => !self.open.is_empty(),
                };
                if !in_queue {
                    self.open.push_back(Open::new(unframed.request));
                }
                // The codec is silent from here: whatever is open ends now.
                while let Some(open) = self.open.pop_front() {
                    self.write(open, Outcome::Unframed);
                }
            }
        }
    }

    /// Routes captured bytes to the exchange they belong to.
    fn captured(&mut self, captured: Captured) {
        let open = match captured {
            Captured::RequestHead(_) | Captured::RequestBody(_) => {
                self.open.iter_mut().rev().find(|o| !o.request_done)
            }
            Captured::ResponseHead(_) | Captured::ResponseBody(_) => self
                .open
                .iter_mut()
                .find(|o| o.status.is_some() && !o.response_done),
        };
        if let Some(capture) = open.and_then(|o| o.capture.as_mut()) {
            capture.push(captured);
        }
    }

    /// Writes the exchanges whose request and response are complete.
    fn reap(&mut self) {
        while self
            .open
            .front()
            .is_some_and(|o| o.request_done && o.response_done)
        {
            if let Some(open) = self.open.pop_front() {
                self.write(open, Outcome::Ok);
            }
        }
    }

    /// The relay ended with `ended`: the exchanges still open end with it. `partial_body` are
    /// the payload bytes read of the request body and of the response body left incomplete.
    pub(crate) fn finish(&mut self, ended: Outcome, partial_body: (u64, u64)) {
        if let Some(open) = self.open.iter_mut().rev().find(|o| !o.request_done) {
            open.bytes_in = partial_body.0;
        }
        if let Some(open) = self
            .open
            .iter_mut()
            .find(|o| o.status.is_some() && !o.response_done)
        {
            open.bytes_out = partial_body.1;
        }
        while let Some(open) = self.open.pop_front() {
            let outcome = match ended {
                Outcome::Ok if open.request_done && open.response_done => Outcome::Ok,
                Outcome::Ok if open.response_done => Outcome::ClientAbort,
                Outcome::Ok => Outcome::UpstreamReset,
                other => other,
            };
            self.write(open, outcome);
        }
    }

    /// Hands an exchange that got a response head to discovery.
    fn discover(&self, open: &mut Open, outcome: Outcome) {
        let (Some(tap), Some(capture)) = (&self.tap, open.capture.take()) else {
            return;
        };
        let (Some(request), Some(status)) = (&open.request, open.status) else {
            return;
        };
        if outcome == Outcome::Unframed {
            return;
        }
        let authority = target_authority(request.uri_authority.as_deref(), request.host.as_deref())
            .map(|raw| normalise_authority(raw, self.ctx.default_port()));
        tap.submit(
            capture,
            Meta {
                port: self.ctx.dst.port(),
                authority,
                status,
                head_ms: open.head.map_or(0, |h| h.as_millis() as u64),
                tunnel: open.tunnel,
            },
        );
    }

    fn write(&self, mut open: Open, outcome: Outcome) {
        self.discover(&mut open, outcome);
        let authority = self.authority(open.request.as_ref());
        if !open.counted {
            self.ctx
                .proxy
                .request_unobserved(self.ctx.direction, PROTOCOL_H1, &authority);
        }
        self.ctx.proxy.bytes(
            self.ctx.direction,
            PROTOCOL_H1,
            open.bytes_in,
            open.bytes_out,
        );
        // Neither duration is meaningful when the codec gave up.
        let timed = outcome != Outcome::Unframed;
        access::exchange(&access::Exchange {
            direction: self.ctx.direction,
            protocol: PROTOCOL_H1,
            dst: self.ctx.dst,
            authority,
            method: open.request.as_ref().map(|r| r.method.clone()),
            path: open
                .request
                .as_ref()
                .filter(|r| !r.path.is_empty())
                .map(|r| r.path.clone()),
            status: open.status,
            grpc_status: None,
            upgrade: open.tunnel,
            tls: self.ctx.tls,
            outcome,
            duration: timed.then(|| open.started.elapsed()),
            head: open.head.filter(|_| timed),
            bytes_in: open.bytes_in,
            bytes_out: open.bytes_out,
        });
    }
}
