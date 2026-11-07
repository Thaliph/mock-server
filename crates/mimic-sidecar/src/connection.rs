//! One redirected connection (spec §4.1): original destination, connect, socket options and
//! protocol detection. Opaque traffic goes to [`crate::opaque`].

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::proxy::authority::normalise_authority;
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

/// TCP keepalive (idle 300 s, interval 30 s, 4 probes) so that a vanished peer is noticed
/// without any idle timeout on live connections.
pub fn configure_socket(stream: &TcpStream) -> io::Result<()> {
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
    let mut abort = pin!(shared.aborted());
    let mut client = client;
    let stall = shared.detect_stall();
    // In mock mode the sidecar connects only once it knows this is not a mocked HTTP/1
    // connection (spec §8): what it read meanwhile is replayed to the detection below.
    let mut read = Vec::new();
    let mock = match shared.mock() {
        Some(mock) if direction == Direction::Outbound && mock.active() => Some(mock.clone()),
        _ => None,
    };
    if let Some(mock) = mock {
        match mock_first(&mut client, stall, abort.as_mut()).await {
            MockFirst::Http1(buf, first) => {
                shared.metrics().count(direction, Outcome::Ok);
                if let Err(e) = configure_socket(&client) {
                    tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot set keepalive");
                }
                mock.connection("mocked");
                let conn = Conn {
                    shared: &shared,
                    direction,
                    dst,
                    started,
                    tls: false,
                    sni: None,
                    interception: None,
                };
                shared
                    .proxy()
                    .connection(direction, "http1", &dst.to_string());
                mock.serve_http1(
                    conn.ctx(),
                    Stream::plain_with_prefix(client, buf),
                    first,
                    shared.stopped(),
                    abort.as_mut(),
                )
                .await;
                return;
            }
            MockFirst::Other(buf) => read = buf,
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
        if let Err(e) = configure_socket(socket) {
            tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot set keepalive");
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
    };
    let mut upstream = upstream;
    let served = match first_bytes(&mut client, &mut upstream, read, stall, abort.as_mut()).await {
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
                        match shared.tls().decide(&hello, dst) {
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
    conn.serve(
        served,
        Stream::plain(client),
        Stream::plain(upstream),
        abort.as_mut(),
    )
    .await;
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

/// What the client said first on an outbound connection in mock mode, before any connection
/// to the destination.
enum MockFirst {
    /// An HTTP/1.x request line, the bytes read, and when the first one arrived.
    Http1(Vec<u8>, Instant),
    /// Anything else: the bytes read so far (possibly none), to detect again once connected.
    Other(Vec<u8>),
}

/// Waits for the first bytes of the client without connecting (spec §8): at most
/// [`MOCK_WAIT`] for the first byte (a server-first protocol), then the usual stall of an
/// incomplete prefix.
async fn mock_first(
    client: &mut TcpStream,
    stall: Duration,
    abort: impl Future<Output = ()>,
) -> MockFirst {
    let mut abort = pin!(abort);
    let mut buf = Vec::new();
    let mut first = None;
    let silent = tokio::time::sleep(MOCK_WAIT);
    let mut silent = pin!(silent);
    loop {
        let mut chunk = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
            () = &mut silent, if buf.is_empty() => return MockFirst::Other(buf),
            () = stalled(!buf.is_empty(), stall) => return MockFirst::Other(buf),
            read = client.read(&mut chunk[..want]) => match read {
                Ok(n) if n > 0 => {
                    first.get_or_insert_with(Instant::now);
                    buf.extend_from_slice(&chunk[..n]);
                    match detect(&buf) {
                        Detection::NeedMore => {}
                        Detection::Http1 => {
                            return MockFirst::Http1(buf, first.unwrap_or_else(Instant::now));
                        }
                        _ => return MockFirst::Other(buf),
                    }
                }
                _ => return MockFirst::Other(buf),
            },
            () = &mut abort => return MockFirst::Other(buf),
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
}

impl Conn<'_> {
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
                let mut tracker = Http1Tracker::new(self.ctx()).with_tap(tap.cloned());
                let relayed = http1::relay_with(
                    &mut client,
                    &mut upstream,
                    buf,
                    tap.is_some(),
                    |event| tracker.event(event),
                    self.shared.stopped(),
                    abort.as_mut(),
                )
                .await;
                tracker.finish(relayed.outcome, relayed.partial_body);
                if let Some(e) = &relayed.error {
                    tracing::debug!(direction = direction.as_str(), %dst, error = %e, "http relay ended");
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
                    r = h2::relay_with(client, upstream, buf, self.ctx(), tap.cloned(), self.shared.stopped()) => r,
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
        let tunnelled = opaque::tunnel(
            &mut client,
            &mut upstream,
            to_upstream,
            to_client,
            early,
            abort.as_mut(),
        )
        .await;
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
            if let Err(e) = configure_socket(&fresh) {
                tracing::debug!(direction = direction.as_str(), %dst, error = %e, "cannot set keepalive");
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
    let served = match decrypted_first_bytes(&mut client, banner_watch, stall, abort.as_mut()).await
    {
        Decrypted::Found(Detection::Http1, buf) => Served::Http1(buf),
        Decrypted::Found(Detection::H2, buf) => Served::H2(buf),
        Decrypted::Found(_, buf) => Served::Tunnel("tcp", buf, Vec::new(), None),
        Decrypted::Closed(buf, early) => Served::Tunnel("tcp", buf, Vec::new(), early),
        Decrypted::Upstream(buf, to_client, early) => Served::Tunnel("tcp", buf, to_client, early),
    };
    conn.serve(served, client, upstream, abort).await;
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
/// `upstream` is given (no ALPN protocol agreed): a banner then ends the detection.
async fn decrypted_first_bytes(
    client: &mut Stream,
    mut upstream: Option<&mut Stream>,
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
            () = stalled(!buf.is_empty(), stall) => {
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
    loop {
        let mut from_client = [0u8; READ_CHUNK];
        let mut from_upstream = [0u8; READ_CHUNK];
        let want = READ_CHUNK.min(DETECT_LIMIT.saturating_sub(buf.len()).max(1));
        tokio::select! {
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
            read = upstream.read(&mut from_upstream) => {
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
            () = &mut abort => {
                return First::Opaque {
                    to_upstream: buf,
                    to_client: Vec::new(),
                    early: Some(Early::Shutdown),
                };
            }
        }
    }
}
