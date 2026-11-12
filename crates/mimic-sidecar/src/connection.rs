//! One redirected connection (spec §4.1): original destination, connect, socket options and
//! protocol detection. Opaque traffic goes to [`crate::opaque`].

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::proxy::authority::{http1_request_host, normalise_authority};
use mimic_core::proxy::tls::{ClientHello, Parsed, Reason, parse_client_hello};
use mimic_core::proxy::{DETECT_LIMIT, Detection, detect};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::access;
use crate::exchange::{Ctx, Http1Tracker};
use crate::h2;
use crate::http1;
use crate::listener::{Active, Direction, Outcome, Shared};
use crate::load::{Marker, Watch};
use crate::mesh::{self, Backend, MeshState, Scope, Serving};
use crate::mock::Mock;
use crate::opaque::{self, Early, Side};
use crate::tls::intercept::{AcceptError, ConnectError, Plan};
use crate::transport::Stream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_IDLE: Duration = Duration::from_secs(300);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_PROBES: u32 = 4;
const READ_CHUNK: usize = 4096;
/// How long a mocked-or-not connection waits for the client's first byte before it connects.
const MOCK_WAIT: Duration = Duration::from_secs(1);
/// Default port of a TLS server name, removed by the authority normalisation.
const TLS_PORT: u16 = 443;

/// `TCP_NODELAY` (HTTP/2 sends small frames back to back: Nagle plus a delayed ACK would add
/// ~40 ms to each exchange, PRX-32) and TCP keepalive (idle 300 s, interval 30 s, 4 probes) so
/// that a vanished peer is noticed without any idle timeout on live connections.
pub fn configure_socket(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_PROBES);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// What the client and the destination said first.
enum First {
    /// The client's bytes so far decided the protocol.
    Client(Detection, Vec<u8>),
    /// The destination spoke first, or a side closed: an opaque tunnel with these bytes
    /// pending towards each side.
    Opaque {
        to_upstream: Vec<u8>,
        to_client: Vec<u8>,
        /// A reset, or the end of the grace, seen while reading.
        early: Option<Early>,
    },
}

pub(crate) async fn handle(
    client: TcpStream,
    direction: Direction,
    shared: Arc<Shared>,
    _active: Active,
) {
    let started = Instant::now();
    let dst = match shared.original_dst(&client) {
        Ok(dst) if !shared.is_listener(dst) => dst,
        Ok(dst) => {
            shared.metrics().count(direction, Outcome::Loop);
            tracing::debug!(direction = direction.as_str(), original_dst = %dst, "loop, connection closed");
            return;
        }
        Err(e) => {
            shared.metrics().count(direction, Outcome::NoOriginalDst);
            tracing::debug!(direction = direction.as_str(), error = %e, "no original destination, connection closed");
            return;
        }
    };
    // One table read and one lookup per outbound connection, nothing per exchange (spec §7.9).
    let mut route = None;
    if direction == Direction::Outbound
        && let Some(mesh) = shared.mesh()
        && mesh.classify(dst) == Scope::InScope
    {
        route = mesh.route(dst).map(|backend| (mesh.clone(), backend));
    }
    let mut abort = pin!(shared.aborted());
    let mut client = client;
    let stall = shared.detect_stall();
    // In mock mode the sidecar connects only once it knows this is not a mocked connection
    // (spec §8): what it read meanwhile is replayed to the detection below.
    let mut read = Vec::new();
    let mock = match shared.mock() {
        Some(mock) if direction == Direction::Outbound && mock.active() => Some(mock.clone()),
        _ => None,
    };
    // Whether a passthrough entry makes this connection one of discovery's.
    let mut passthrough = false;
    if let Some(mock) = &mock {
        match mock_first(&mut client, mock, dst, stall, abort.as_mut()).await {
            MockFirst::Route(route) => {
                serve_mock(&shared, mock, dst, started, client, route, abort.as_mut()).await;
                return;
            }
            MockFirst::Passthrough(buf) => {
                mock.connection("passthrough");
                passthrough = true;
                read = buf;
            }
            MockFirst::Other(buf) => read = buf,
            MockFirst::Gone => return,
        }
    }
    // An in-scope Service is balanced per request when the client speaks cleartext HTTP (spec
    // §7.9): what it sends first decides, so the original destination is not dialled yet.
    let route = if mock.is_none() { route } else { None };
    if let Some((mesh, backend)) = route {
        match mesh_first(&mut client, stall, abort.as_mut()).await {
            MeshFirst::Http(h2, buf) => {
                let routed = Routed {
                    mesh,
                    backend,
                    dst,
                    started,
                };
                serve_mesh(&shared, routed, client, h2, buf, abort.as_mut()).await;
                return;
            }
            MeshFirst::Other(buf) => read = buf,
            MeshFirst::Gone => return,
        }
    }
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await;
    let upstream = match connected {
        Ok(Ok(upstream)) => upstream,
        failure => {
            shared.metrics().count(direction, Outcome::ConnectError);
            let error = match failure {
                Ok(Err(e)) => e.to_string(),
                _ => "connect timeout".to_string(),
            };
            tracing::debug!(direction = direction.as_str(), %dst, %error, "connect failed");
            return;
        }
    };
    shared.metrics().count(direction, Outcome::Ok);
    for socket in [&client, &upstream] {
        if let Err(e) = shared.prepare_socket(socket) {
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot configure socket");
        }
    }
    let mut conn = Conn {
        shared: &shared,
        direction,
        dst,
        started,
        tls: false,
        sni: None,
        interception: None,
        mock: None,
    };
    let mut upstream = upstream;
    let first = first_bytes(&mut client, &mut upstream, read, stall, abort.as_mut()).await;
    // The client spoke HTTP or TLS before the destination did: the mock answers, and the
    // destination socket is closed without a byte written to it (spec §8).
    let first = match (first, &mock) {
        (First::Client(detection, mut buf), Some(mock))
            if !passthrough
                && matches!(
                    detection,
                    Detection::Http1 | Detection::H2 | Detection::Tls { .. }
                ) =>
        {
            // With a passthrough list the first request head is read whole (within the
            // stall) to know its authority, as before connecting.
            if matches!(pick(mock, &detection, &buf, dst, true), Pick::More) {
                let deadline = tokio::time::Instant::now() + stall;
                hold_head(&mut client, &mut buf, deadline, abort.as_mut()).await;
            }
            match pick(mock, &detection, &buf, dst, false) {
                Pick::Passthrough => {
                    mock.connection("passthrough");
                    passthrough = true;
                    First::Client(detection, buf)
                }
                _ => {
                    drop(upstream);
                    let route = MockRoute::new(detection, buf);
                    serve_mock(&shared, mock, dst, started, client, route, abort.as_mut()).await;
                    return;
                }
            }
        }
        (first, _) => first,
    };
    if direction == Direction::Outbound
        && !passthrough
        && let (Some(mock), First::Client(detection, buf)) = (shared.mock(), &first)
        && !matches!(pick(mock, detection, buf, dst, false), Pick::Passthrough)
    {
        conn.mock = Some(mock.clone());
    }
    let served = match first {
        First::Client(detection, buf) => {
            tracing::debug!(direction = direction.as_str(), %dst, ?detection, "protocol detected");
            match detection {
                Detection::Http1 => Served::Http1(buf),
                Detection::H2 => Served::H2(buf),
                Detection::Tls { sni } => {
                    conn.sni = sni.map(|name| normalise_authority(&name, TLS_PORT));
                    if direction == Direction::Outbound {
                        let hello = match parse_client_hello(&buf) {
                            Parsed::Hello(hello) => hello,
                            _ => ClientHello::default(),
                        };
                        let decided = if shared.mock().is_some_and(Mock::silent) {
                            Err(Reason::ModeNone)
                        } else {
                            shared.tls().decide(&hello, dst)
                        };
                        match decided {
                            Ok(plan) => {
                                intercept(conn, client, upstream, buf, plan, abort.as_mut()).await;
                                return;
                            }
                            Err(reason) => {
                                shared.tls().count(reason.as_str());
                                conn.interception = Some(reason.as_str());
                            }
                        }
                    }
                    Served::Tunnel("tls", buf, Vec::new(), None)
                }
                _ => Served::Tunnel("tcp", buf, Vec::new(), None),
            }
        }
        First::Opaque {
            to_upstream,
            to_client,
            early,
        } => Served::Tunnel("tcp", to_upstream, to_client, early),
    };
    if let Some(mock) = &mock
        && !passthrough
        && matches!(served, Served::Tunnel(..))
    {
        mock.connection("tunnel");
    }
    conn.serve(
        served,
        Stream::plain(client),
        Stream::plain(upstream),
        abort.as_mut(),
    )
    .await;
}

/// An in-scope connection the mesh balances.
struct Routed {
    mesh: MeshState,
    backend: Arc<Backend>,
    dst: SocketAddr,
    started: Instant,
}

/// Serves a connection per request over the pods of its Service (spec §7.9).
async fn serve_mesh(
    shared: &Arc<Shared>,
    routed: Routed,
    client: TcpStream,
    h2: bool,
    first: Vec<u8>,
    abort: Pin<&mut (impl Future<Output = ()> + Send)>,
) {
    let direction = Direction::Outbound;
    let Routed {
        mesh,
        backend,
        dst,
        started,
    } = routed;
    shared.metrics().count(direction, Outcome::Ok);
    if let Err(e) = shared.prepare_socket(&client) {
        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot configure socket");
    }
    let conn = Conn {
        shared,
        direction,
        dst,
        started,
        tls: false,
        sni: None,
        interception: None,
        mock: None,
    };
    shared.proxy().connection(
        direction,
        if h2 { "h2" } else { "http1" },
        &conn.authority(false),
    );
    let serving = Serving {
        mesh,
        backend,
        ctx: conn.ctx(),
        tap: shared.discovery().cloned(),
    };
    let stream = Stream::plain_with_prefix(client, first);
    if let Err(e) = mesh::serve(serving, stream, h2, shared.stopped(), abort).await {
        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "mesh connection ended");
    }
}

/// What the client of an in-scope connection said first.
enum MeshFirst {
    /// Cleartext HTTP the mesh balances: `true` for HTTP/2, and the bytes read.
    Http(bool, Vec<u8>),
    /// Anything else, or an HTTP/1 head that must stay on the byte relay: the bytes read so
    /// far (possibly none), to detect again once connected.
    Other(Vec<u8>),
    /// The client closed, failed or was told to stop: nothing is dialled.
    Gone,
}

/// Waits for the first bytes of the client without connecting: at most [`MOCK_WAIT`] for the
/// first byte (a server-first protocol), then the usual stall of an incomplete prefix, and for
/// an HTTP/1 request until its head is whole.
async fn mesh_first(
    client: &mut TcpStream,
    stall: Duration,
    abort: impl Future<Output = ()>,
) -> MeshFirst {
    let mut abort = pin!(abort);
    let mut buf = Vec::new();
    let silent = tokio::time::sleep(MOCK_WAIT);
    let mut silent = pin!(silent);
    let deadline = tokio::time::sleep(stall);
    let mut deadline = pin!(deadline);
    loop {
        let mut chunk = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            () = &mut silent, if buf.is_empty() => return MeshFirst::Other(buf),
            () = &mut deadline, if !buf.is_empty() => return MeshFirst::Other(buf),
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => {
                    if buf.is_empty() {
                        deadline.as_mut().reset(tokio::time::Instant::now() + stall);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    match detect(&buf) {
                        Detection::NeedMore => {}
                        Detection::H2 => return MeshFirst::Http(true, buf),
                        Detection::Http1 => match mesh::head_kind(&buf) {
                            mesh::Head::Partial => {}
                            mesh::Head::Relay => return MeshFirst::Other(buf),
                            mesh::Head::Routed => return MeshFirst::Http(false, buf),
                        },
                        _ => return MeshFirst::Other(buf),
                    }
                    if buf.len() >= DETECT_LIMIT {
                        return MeshFirst::Other(buf);
                    }
                }
                _ => return MeshFirst::Gone,
            },
            () = &mut abort => return MeshFirst::Gone,
        }
    }
}

/// What the mock does with a connection whose first bytes are HTTP or TLS.
enum MockRoute {
    /// An HTTP/1.x request line or the HTTP/2 preface (`true`), the bytes read, and when the
    /// first one arrived.
    Http(bool, Vec<u8>, Instant),
    /// A TLS ClientHello, with its SNI and the bytes read: answered by the mock (spec §5).
    Tls(Option<String>, Vec<u8>),
}

impl MockRoute {
    fn new(detection: Detection, buf: Vec<u8>) -> MockRoute {
        MockRoute::at(detection, buf, Instant::now())
    }

    fn at(detection: Detection, buf: Vec<u8>, first: Instant) -> MockRoute {
        match detection {
            Detection::Tls { sni } => MockRoute::Tls(sni, buf),
            detection => MockRoute::Http(detection == Detection::H2, buf, first),
        }
    }
}

/// Serves a mocked connection (HTTP or intercepted TLS), with no connection to the destination.
async fn serve_mock(
    shared: &Arc<Shared>,
    mock: &Mock,
    dst: SocketAddr,
    started: Instant,
    client: TcpStream,
    route: MockRoute,
    mut abort: Pin<&mut (impl Future<Output = ()> + Send)>,
) {
    let direction = Direction::Outbound;
    shared.metrics().count(direction, Outcome::Ok);
    if let Err(e) = shared.prepare_socket(&client) {
        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot configure socket");
    }
    let mut conn = Conn {
        shared,
        direction,
        dst,
        started,
        tls: false,
        sni: None,
        interception: None,
        mock: None,
    };
    match route {
        MockRoute::Http(h2, buf, first) => {
            mock.connection("mocked");
            let protocol = if h2 { "h2" } else { "http1" };
            shared
                .proxy()
                .connection(direction, protocol, &dst.to_string());
            let stream = Stream::plain_with_prefix(client, buf);
            if h2 {
                mock.serve_h2(conn.ctx(), stream, shared.stopped(), abort.as_mut())
                    .await;
            } else {
                mock.serve_http1(conn.ctx(), stream, first, shared.stopped(), abort.as_mut())
                    .await;
            }
        }
        MockRoute::Tls(sni, buf) => {
            conn.sni = sni.map(|name| normalise_authority(&name, TLS_PORT));
            mock_tls(conn, mock, client, buf, abort.as_mut()).await;
        }
    }
}

/// Whether a connection whose first bytes are `detection` is mocked or passed through
/// (`MIMIC_MOCK_PASSTHROUGH`, spec §8), or needs more bytes to tell.
enum Pick {
    Mock,
    Passthrough,
    More,
}

/// With `wait_head`, an HTTP/1 connection is undecided until its request head is complete.
fn pick(mock: &Mock, detection: &Detection, buf: &[u8], dst: SocketAddr, wait_head: bool) -> Pick {
    let list = mock.passthrough();
    if list.is_empty() {
        return Pick::Mock;
    }
    let matched = match detection {
        Detection::Tls { sni } => list.matches(sni.as_deref(), dst.ip()),
        Detection::Http1 => {
            if list.matches(None, dst.ip()) {
                return Pick::Passthrough;
            }
            let complete = buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= DETECT_LIMIT;
            if wait_head && !complete {
                return Pick::More;
            }
            match http1_request_host(buf) {
                Some(host) => match host.parse::<IpAddr>() {
                    Ok(ip) => list.matches(None, ip),
                    Err(_) => list.matches(Some(&host), dst.ip()),
                },
                None => false,
            }
        }
        _ => list.matches(None, dst.ip()),
    };
    if matched {
        Pick::Passthrough
    } else {
        Pick::Mock
    }
}

/// The tunnel kind of an outbound opaque connection (spec §6.1): `tls_<reason>` when §5 passed
/// it through, else `tcp`.
fn tunnel_kind(interception: Option<&str>) -> &'static str {
    match interception {
        Some("excluded") => "tls_excluded",
        Some("no_ca") => "tls_no_ca",
        Some("unsupported") => "tls_unsupported",
        Some("upstream_untrusted") => "tls_upstream_untrusted",
        Some("client_cert_requested") => "tls_client_cert_requested",
        Some("client_rejected") => "tls_client_rejected",
        _ => "tcp",
    }
}

/// Reads from `client` until `buf` holds a whole HTTP/1 request head, the detection limit is
/// reached, or `deadline` passes, the client ends or the connection is aborted.
async fn hold_head(
    client: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: tokio::time::Instant,
    abort: impl Future<Output = ()>,
) {
    let mut abort = pin!(abort);
    let deadline = tokio::time::sleep_until(deadline);
    let mut deadline = pin!(deadline);
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < DETECT_LIMIT {
        let mut chunk = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT - buf.len());
        tokio::select! {
            () = &mut deadline => return,
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                _ => return,
            },
            () = &mut abort => return,
        }
    }
}

/// What the client said first on an outbound connection in mock mode, before any connection
/// to the destination.
enum MockFirst {
    /// Served by the mock.
    Route(MockRoute),
    /// Handled as in discovery (`MIMIC_MOCK_PASSTHROUGH`): the bytes read, to detect again once
    /// connected.
    Passthrough(Vec<u8>),
    /// Anything else: the bytes read so far (possibly none), to detect again once connected.
    Other(Vec<u8>),
    /// The client closed, failed or was told to stop: nothing is dialled.
    Gone,
}

/// Waits for the first bytes of the client without connecting (spec §8): at most
/// [`MOCK_WAIT`] for the first byte (a server-first protocol), then the usual stall of an
/// incomplete prefix.
async fn mock_first(
    client: &mut TcpStream,
    mock: &Mock,
    dst: SocketAddr,
    stall: Duration,
    abort: impl Future<Output = ()>,
) -> MockFirst {
    let mut abort = pin!(abort);
    let mut buf = Vec::new();
    let mut first = None;
    let silent = tokio::time::sleep(MOCK_WAIT);
    let mut silent = pin!(silent);
    // One deadline from the first byte, not one per read: a client trickling bytes is cut too.
    let deadline = tokio::time::sleep(stall);
    let mut deadline = pin!(deadline);
    loop {
        let mut chunk = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            () = &mut silent, if buf.is_empty() => return MockFirst::Other(buf),
            () = &mut deadline, if !buf.is_empty() => {
                // An HTTP/1 head still incomplete: decided on what it holds, and the mock's own
                // header timeout deals with the rest. Any other incomplete prefix is reset.
                let detection = detect(&buf);
                if !matches!(detection, Detection::Http1) {
                    return MockFirst::Gone;
                }
                if matches!(pick(mock, &detection, &buf, dst, false), Pick::Passthrough) {
                    return MockFirst::Passthrough(buf);
                }
                return MockFirst::Route(MockRoute::Http(
                    false,
                    buf,
                    first.unwrap_or_else(Instant::now),
                ));
            }
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => {
                    if first.is_none() {
                        first = Some(Instant::now());
                        deadline.as_mut().reset(tokio::time::Instant::now() + stall);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let detection = detect(&buf);
                    if matches!(detection, Detection::NeedMore) {
                        continue;
                    }
                    if !matches!(detection, Detection::Http1 | Detection::H2 | Detection::Tls { .. }) {
                        return MockFirst::Other(buf);
                    }
                    match pick(mock, &detection, &buf, dst, true) {
                        Pick::More => {}
                        Pick::Passthrough => return MockFirst::Passthrough(buf),
                        Pick::Mock => {
                            let at = first.unwrap_or_else(Instant::now);
                            return MockFirst::Route(MockRoute::at(detection, buf, at));
                        }
                    }
                }
                _ => return MockFirst::Gone,
            },
            () = &mut abort => return MockFirst::Gone,
        }
    }
}

/// What the first bytes decided, to relay.
enum Served {
    Http1(Vec<u8>),
    H2(Vec<u8>),
    /// An opaque tunnel of this protocol, with the bytes pending towards each side.
    Tunnel(&'static str, Vec<u8>, Vec<u8>, Option<Early>),
}

/// One connection being served.
struct Conn<'a> {
    shared: &'a Arc<Shared>,
    direction: Direction,
    dst: SocketAddr,
    started: Instant,
    /// The streams are the decrypted ones of an interception (spec §5).
    tls: bool,
    /// The normalised SNI of a TLS connection, when there is one.
    sni: Option<String>,
    /// Why an outbound TLS connection is not intercepted.
    interception: Option<&'static str>,
    /// Set on a forwarded outbound connection that follows a switch to mock (spec §8
    /// "Switching"); not on `MIMIC_MOCK_PASSTHROUGH` ones, nor on non-HTTP cleartext tunnels.
    mock: Option<Mock>,
}

impl Conn<'_> {
    /// The load marker check, on inbound connections (spec §7.8).
    fn load(&self) -> Option<&Marker> {
        self.shared
            .load()
            .filter(|_| self.direction == Direction::Inbound)
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            direction: self.direction,
            dst: self.dst,
            tls: self.tls,
            sni: self.sni.clone(),
            proxy: self.shared.proxy().clone(),
            aborted: self.shared.abort_flag(),
        }
    }

    /// The `authority` of the connection metrics: the SNI of a decrypted connection and of a
    /// tunnel, the destination of a clear HTTP connection (as always).
    fn authority(&self, tunnel: bool) -> String {
        match &self.sni {
            Some(sni) if tunnel || self.tls => sni.clone(),
            _ => self.dst.to_string(),
        }
    }

    async fn serve(
        &self,
        served: Served,
        mut client: Stream,
        mut upstream: Stream,
        mut abort: Pin<&mut (impl Future<Output = ()> + Send)>,
    ) {
        let (direction, dst) = (self.direction, self.dst);
        let proxy = self.shared.proxy();
        let (protocol, to_upstream, to_client, early) = match served {
            Served::Http1(buf) => {
                proxy.connection(direction, "http1", &self.authority(false));
                // At the end of the grace the relay closes both sockets, and the exchanges
                // still open write their `shutdown` line.
                // The exchanges the deployment serves, and those it sends, decrypted or not
                // (spec §6, §6.1).
                let tap = self.shared.discovery();
                let watch = self.load().map(|marker| {
                    Watch::new(marker.clone(), tap.cloned(), self.shared.detect_stall())
                });
                let mut tracker = Http1Tracker::new(self.ctx())
                    .with_tap(tap.cloned())
                    .with_watch(watch.clone());
                let relayed = http1::relay_with(
                    &mut client,
                    &mut upstream,
                    buf,
                    tap.is_some(),
                    self.mock.clone(),
                    watch,
                    Some(proxy.overhead_histogram(direction)),
                    |event| tracker.event(event),
                    self.shared.stopped(),
                    abort.as_mut(),
                )
                .await;
                tracker.finish(relayed.outcome, relayed.partial_body);
                if let Some(e) = &relayed.error {
                    tracing::debug!(direction = direction.as_str(), %dst, error = %e, "http relay ended");
                }
                if let Some(refusal) = relayed.refused {
                    // A load request the marker check refused (spec §4.1): nothing of it
                    // reached the destination, whose connection is closed.
                    drop(upstream);
                    http1::refuse(&mut client, refusal).await;
                    return;
                }
                if let Some(mock) = &self.mock {
                    if relayed.outcome == access::Outcome::MockClosed {
                        mock.connection("closed");
                    }
                    if let Some((bytes, first)) = relayed.handover {
                        // The destination connection is closed; the client's next request and
                        // all after it are the mock's (spec §8 "Switching").
                        drop(upstream);
                        mock.connection("mocked");
                        mock.serve_http1(
                            self.ctx(),
                            client.with_prefix(bytes),
                            first,
                            self.shared.stopped(),
                            abort.as_mut(),
                        )
                        .await;
                        return;
                    }
                }
                if relayed.outcome == access::Outcome::Shutdown {
                    tracing::debug!(direction = direction.as_str(), %dst, unanswered = relayed.unanswered, "http relay closed at the end of the grace");
                }
                return;
            }
            Served::H2(buf) => {
                proxy.connection(direction, "h2", &self.authority(false));
                let tap = self.shared.discovery();
                let relayed = tokio::select! {
                    r = h2::relay_with(client, upstream, buf, self.ctx(), (tap.cloned(), self.mock.clone(), self.load().cloned()), self.shared.stopped()) => r,
                    () = abort.as_mut() => Ok(()),
                };
                if let Err(e) = relayed {
                    tracing::debug!(direction = direction.as_str(), %dst, error = %e, "h2 relay ended");
                }
                return;
            }
            Served::Tunnel(protocol, to_upstream, to_client, early) => {
                (protocol, to_upstream, to_client, early)
            }
        };
        proxy.connection(direction, protocol, &self.authority(true));
        // Counted once its kind is decided (spec §6.1).
        if direction == Direction::Outbound
            && let Some(tap) = self.shared.discovery()
        {
            let kind = tunnel_kind(self.interception);
            tap.tunnel(kind, dst, self.tls, self.sni.clone(), None);
        }
        // TLS passed through and decrypted opaque streams are not kept by the mock; non-HTTP
        // cleartext tunnels are (spec §8 "Switching").
        let closes = self.mock.as_ref().filter(|_| protocol == "tls" || self.tls);
        let tunnelled = opaque::tunnel(
            &mut client,
            &mut upstream,
            to_upstream,
            to_client,
            early,
            abort.as_mut(),
            async {
                match closes {
                    Some(mock) => mock.entered().await,
                    None => std::future::pending().await,
                }
            },
        )
        .await;
        if tunnelled.outcome == access::Outcome::MockClosed
            && let Some(mock) = closes
        {
            mock.connection("closed");
        }
        if let Some(e) = &tunnelled.error {
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "tunnel ended");
        }
        proxy.bytes(
            direction,
            protocol,
            tunnelled.to_upstream,
            tunnelled.to_client,
        );
        access::connection(&access::Connection {
            direction,
            protocol,
            dst,
            sni: self.sni.clone(),
            interception: self.interception,
            outcome: tunnelled.outcome,
            duration: self.started.elapsed(),
            bytes_in: tunnelled.to_upstream,
            bytes_out: tunnelled.to_client,
        });
    }
}

/// An outbound TLS connection the sidecar intercepts (spec §5): the handshake with the
/// destination first, then the client's, then the decrypted streams go through the same
/// detection and relays as cleartext.
async fn intercept(
    mut conn: Conn<'_>,
    client: TcpStream,
    upstream: TcpStream,
    hello: Vec<u8>,
    plan: Plan,
    mut abort: Pin<&mut (impl Future<Output = ()> + Send)>,
) {
    let (direction, dst) = (conn.direction, conn.dst);
    let tls = conn.shared.tls();
    let upstream = match tls.connect(upstream, &plan).await {
        Ok(upstream) => upstream,
        Err(ConnectError::Reason(reason)) => {
            // Pass through: a fresh connection receives the ClientHello again, untouched.
            tls.passthrough(&plan, reason, dst);
            conn.interception = Some(reason.as_str());
            let fresh = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await {
                Ok(Ok(fresh)) => fresh,
                _ => {
                    tracing::debug!(direction = direction.as_str(), %dst, "pass-through connect failed");
                    return;
                }
            };
            if let Err(e) = conn.shared.prepare_socket(&fresh) {
                tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot configure socket");
            }
            let served = Served::Tunnel("tls", hello, Vec::new(), None);
            conn.serve(served, Stream::plain(client), Stream::plain(fresh), abort)
                .await;
            return;
        }
        Err(ConnectError::Io(e)) => {
            // The client sees what the destination did, and nothing is remembered.
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "handshake with the destination failed");
            opaque::abort(&client);
            return;
        }
    };
    let alpn = upstream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    let alpn_agreed = alpn.is_some();
    let client = match tls.accept(client, hello, &plan, alpn).await {
        Ok(client) => client,
        Err(AcceptError::Rejected) => {
            // The client gave up on this connection; the next one passes through.
            tracing::debug!(direction = direction.as_str(), %dst, "client rejected the certificate");
            tls.passthrough(&plan, Reason::ClientRejected, dst);
            if let Some(tap) = conn.shared.discovery() {
                tap.tunnel("tls_client_rejected", dst, false, conn.sni.clone(), None);
            }
            return;
        }
        Err(AcceptError::Failed(e)) => {
            // A reset, a timeout: nothing is learned about the client's trust.
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "handshake with the client failed");
            return;
        }
    };
    tls.count("intercepted");
    conn.tls = true;
    let (mut client, mut upstream) = (Stream::tls(client.into()), Stream::tls(upstream.into()));
    // With an ALPN protocol agreed, the client speaks first (an h2 destination sends SETTINGS
    // at once, which is not a banner); without, a destination that speaks first is a banner
    // protocol and its bytes make an opaque tunnel.
    let banner_watch = (!alpn_agreed).then_some(&mut upstream);
    let stall = conn.shared.detect_stall();
    let served = match decrypted_first_bytes(
        &mut client,
        banner_watch,
        false,
        stall,
        abort.as_mut(),
    )
    .await
    {
        Decrypted::Found(Detection::Http1, buf) => Served::Http1(buf),
        Decrypted::Found(Detection::H2, buf) => Served::H2(buf),
        Decrypted::Found(_, buf) => Served::Tunnel("tcp", buf, Vec::new(), None),
        Decrypted::Closed(buf, early) => Served::Tunnel("tcp", buf, Vec::new(), early),
        Decrypted::Upstream(buf, to_client, early) => Served::Tunnel("tcp", buf, to_client, early),
    };
    conn.serve(served, client, upstream, abort).await;
}

/// An outbound TLS connection in mock mode (spec §5 "Mock mode"): the leaf for the SNI is
/// issued without any handshake with the destination, and a decrypted HTTP stream is a mocked
/// connection. Whatever the mock cannot answer is closed, with no byte to the destination.
async fn mock_tls(
    mut conn: Conn<'_>,
    mock: &Mock,
    client: TcpStream,
    hello_bytes: Vec<u8>,
    mut abort: Pin<&mut (impl Future<Output = ()> + Send)>,
) {
    let (direction, dst) = (conn.direction, conn.dst);
    let shared = conn.shared;
    let tls = shared.tls();
    let hello = match parse_client_hello(&hello_bytes) {
        Parsed::Hello(hello) => hello,
        _ => ClientHello::default(),
    };
    let hello_len = hello_bytes.len() as u64;
    let plan = match tls.decide(&hello, dst) {
        Ok(plan) => plan,
        Err(reason) => {
            tls.count(reason.as_str());
            conn.interception = Some(reason.as_str());
            conn.mock_closed(mock, "tls", hello_len);
            return;
        }
    };
    let server = match tls
        .accept(client, hello_bytes, &plan, plan.mock_alpn())
        .await
    {
        Ok(server) => server,
        Err(AcceptError::Rejected) => {
            // The client does not trust the leaf: as in discovery, the next connection to this
            // destination is not intercepted.
            tracing::debug!(direction = direction.as_str(), %dst, "client rejected the certificate");
            tls.passthrough(&plan, Reason::ClientRejected, dst);
            conn.interception = Some(Reason::ClientRejected.as_str());
            conn.mock_closed(mock, "tls", hello_len);
            return;
        }
        Err(AcceptError::Failed(e)) => {
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "handshake with the client failed");
            return;
        }
    };
    tls.count("intercepted");
    conn.tls = true;
    let mut stream = Stream::tls(server.into());
    let first = Instant::now();
    let stall = shared.detect_stall();
    let detected = decrypted_first_bytes(&mut stream, None, true, stall, abort.as_mut()).await;
    let (h2, buf) = match detected {
        Decrypted::Found(Detection::H2, buf) => (true, buf),
        Decrypted::Found(Detection::Http1, buf) => (false, buf),
        Decrypted::Found(_, buf) | Decrypted::Closed(buf, _) | Decrypted::Upstream(buf, _, _) => {
            conn.mock_closed(mock, "tcp", buf.len() as u64);
            return;
        }
    };
    mock.connection("mocked");
    let protocol = if h2 { "h2" } else { "http1" };
    shared
        .proxy()
        .connection(direction, protocol, &conn.authority(false));
    let stream = stream.with_prefix(buf);
    if h2 {
        mock.serve_h2(conn.ctx(), stream, shared.stopped(), abort.as_mut())
            .await;
    } else {
        mock.serve_http1(conn.ctx(), stream, first, shared.stopped(), abort.as_mut())
            .await;
    }
}

impl Conn<'_> {
    /// A connection the mock closes (spec §8): counted and written as a `connection` line with
    /// `result: mock_closed`; nothing was sent to the destination.
    fn mock_closed(&self, mock: &Mock, protocol: &'static str, bytes_in: u64) {
        mock.connection("closed");
        self.shared
            .proxy()
            .connection(self.direction, protocol, &self.authority(true));
        access::connection(&access::Connection {
            direction: self.direction,
            protocol,
            dst: self.dst,
            sni: self.sni.clone(),
            interception: self.interception,
            outcome: access::Outcome::MockClosed,
            duration: self.started.elapsed(),
            bytes_in,
            bytes_out: 0,
        });
    }
}

enum Decrypted {
    Found(Detection, Vec<u8>),
    /// The client closed or failed, or the grace ended, before a protocol was known.
    Closed(Vec<u8>, Option<Early>),
    /// The destination spoke first: the bytes read from the client so far, those from the
    /// destination, and how the destination side ended, if it did.
    Upstream(Vec<u8>, Vec<u8>, Option<Early>),
}

/// [`first_bytes`] for the decrypted client stream. The destination is read only when
/// `upstream` is given (no ALPN protocol agreed): a banner then ends the detection. With no
/// destination connected (`silent_closes`), a client silent for `stall` is closed too.
async fn decrypted_first_bytes(
    client: &mut Stream,
    mut upstream: Option<&mut Stream>,
    silent_closes: bool,
    stall: Duration,
    abort: impl Future<Output = ()>,
) -> Decrypted {
    let mut abort = pin!(abort);
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; READ_CHUNK];
        let mut banner = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            () = stalled(silent_closes || !buf.is_empty(), stall) => {
                let timeout = io::Error::new(io::ErrorKind::TimedOut, "stalled on a protocol prefix");
                return Decrypted::Closed(buf, Some(Early::Failed(Side::Client, timeout)));
            }
            read = read_watch(upstream.as_deref_mut(), &mut banner) => {
                let (n, early) = match read {
                    Ok(n) => (n, None),
                    Err(e) => (0, Some(Early::Failed(Side::Upstream, e))),
                };
                return Decrypted::Upstream(buf, banner[..n].to_vec(), early);
            }
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => {
                    buf.extend_from_slice(&chunk[..n]);
                    match detect(&buf) {
                        Detection::NeedMore => {}
                        found => return Decrypted::Found(found, buf),
                    }
                }
                Ok(_) => return Decrypted::Closed(buf, None),
                Err(e) => {
                    return Decrypted::Closed(buf, Some(Early::Failed(Side::Client, e)));
                }
            },
            () = &mut abort => return Decrypted::Closed(buf, Some(Early::Shutdown)),
        }
    }
}

/// Resolves after `stall` when `pending_bytes`; never otherwise: a connection that has sent
/// nothing may be waiting for a server-first protocol (spec §4.1).
async fn stalled(pending_bytes: bool, stall: Duration) {
    if pending_bytes {
        tokio::time::sleep(stall).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// A read of the destination, when it is watched; never ready otherwise.
async fn read_watch(upstream: Option<&mut Stream>, into: &mut [u8]) -> io::Result<usize> {
    match upstream {
        Some(upstream) => upstream.read(into).await,
        None => std::future::pending().await,
    }
}

/// Whether `bytes`, the start of what a destination wrote first, can be the header of the
/// initial SETTINGS frame of an HTTP/2 server: a length that is a multiple of 6 up to 16384,
/// type 4, no flag, stream 0.
fn could_be_h2_settings(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let fits = |i: usize, ok: &dyn Fn(u8) -> bool| bytes.get(i).is_none_or(|b| ok(*b));
    let length_ok = if bytes.len() >= 3 {
        let length =
            usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2]);
        length <= 16384 && length % 6 == 0
    } else {
        fits(0, &|b| b == 0) && fits(1, &|b| b <= 0x40)
    };
    length_ok
        && fits(3, &|b| b == 4)
        && fits(4, &|b| b == 0)
        && (5..9).all(|i| fits(i, &|b| b == 0))
}

/// Reads until the protocol is known, within [`DETECT_LIMIT`] and with no timeout but the
/// stall of an incomplete prefix. The bytes
/// read are returned to be forwarded unchanged. A reset on either side is returned as such, to
/// reach the other side as a reset; the end of the grace (`abort`) ends the detection.
async fn first_bytes(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    mut buf: Vec<u8>,
    stall: Duration,
    abort: impl Future<Output = ()>,
) -> First {
    let mut abort = pin!(abort);
    // Bytes read before the connection to the destination existed (mock mode).
    if !buf.is_empty() {
        match detect(&buf) {
            Detection::NeedMore => {}
            found => return First::Client(found, buf),
        }
    }
    let mut graced = false;
    loop {
        let mut from_client = [0u8; READ_CHUNK];
        let mut from_upstream = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        // The client first when both are ready: an HTTP/2 server writes its SETTINGS before
        // the client's preface, and the client's bytes are the ones that name the protocol.
        tokio::select! {
            biased;
            () = &mut abort => {
                return First::Opaque {
                    to_upstream: buf,
                    to_client: Vec::new(),
                    early: Some(Early::Shutdown),
                };
            }
            () = stalled(!buf.is_empty(), stall) => {
                let timeout = io::Error::new(io::ErrorKind::TimedOut, "stalled on a protocol prefix");
                let early = Some(Early::Failed(Side::Client, timeout));
                return First::Opaque { to_upstream: buf, to_client: Vec::new(), early };
            }
            read = client.read(&mut from_client[..want]) => match read {
                Ok(n) if n > 0 => {
                    buf.extend_from_slice(&from_client[..n]);
                    match detect(&buf) {
                        Detection::NeedMore => {}
                        found => return First::Client(found, buf),
                    }
                }
                // Closed: the tunnel propagates the FIN after the pending bytes.
                Ok(_) => return First::Opaque { to_upstream: buf, to_client: Vec::new(), early: None },
                Err(e) => {
                    let early = Some(Early::Failed(Side::Client, e));
                    return First::Opaque { to_upstream: buf, to_client: Vec::new(), early };
                }
            },
            // The destination wrote first: a server-first protocol, tunnelled at once, unless it
            // wrote the SETTINGS frame of an HTTP/2 server (spec §4.1), which comes before the
            // client's preface: then the client's first bytes are waited for, as without mimic.
            _ = upstream.readable() => {
                if !graced {
                    graced = true;
                    let mut head = [0u8; 9];
                    // A reset is an answer of its own: it reaches the client as one.
                    let peeked = match upstream.peek(&mut head).await {
                        Ok(n) => n,
                        Err(e) => {
                            return First::Opaque {
                                to_upstream: buf,
                                to_client: Vec::new(),
                                early: Some(Early::Failed(Side::Upstream, e)),
                            };
                        }
                    };
                    if could_be_h2_settings(&head[..peeked]) {
                        // Bounded by the stall, and a shutdown ends the wait (spec §4.1).
                        let waited = tokio::select! {
                            biased;
                            () = &mut abort => {
                                return First::Opaque {
                                    to_upstream: buf,
                                    to_client: Vec::new(),
                                    early: Some(Early::Shutdown),
                                };
                            }
                            ready = tokio::time::timeout(stall, client.readable()) => ready.is_ok(),
                        };
                        if waited {
                            continue;
                        }
                    }
                }
                let read = upstream.read(&mut from_upstream).await;
                let (n, early) = match read {
                    Ok(n) => (n, None),
                    Err(e) => (0, Some(Early::Failed(Side::Upstream, e))),
                };
                return First::Opaque {
                    to_upstream: buf,
                    to_client: from_upstream[..n].to_vec(),
                    early,
                };
            }
        }
    }
}

#[cfg(test)]
mod settings_tests {
    use super::could_be_h2_settings;

    #[test]
    fn grp_37_only_the_header_of_a_settings_frame_is_one() {
        for ok in [
            &[0, 0, 0, 4, 0, 0, 0, 0, 0][..],
            &[0, 0, 6, 4, 0, 0, 0, 0, 0],
            &[0, 0, 18, 4, 0, 0, 0, 0, 0],
            &[0, 0, 18, 4, 0, 0, 0, 0, 0, 0, 3],
            &[0],
            &[0, 0, 6, 4],
        ] {
            assert!(could_be_h2_settings(ok), "{ok:?}");
        }
        for bad in [
            &[][..],
            &[0, 0, 0, 1, 0, 0, 0, 0, 0],
            &[0, 0, 0, 4, 1, 0, 0, 0, 0],
            &[0, 0, 0, 4, 0, 0, 0, 0, 1],
            &[0, 0, 7, 4, 0, 0, 0, 0, 0],
            &[0, 0x40, 6, 4, 0, 0, 0, 0, 0],
            &[1, 0, 0, 4, 0, 0, 0, 0, 0],
            b"220 hello\r\n",
            b"SSH-2.0-x\r\n",
            &[0x4a, 0, 0, 0, 0],
        ] {
            assert!(!could_be_h2_settings(bad), "{bad:?}");
        }
    }
}
