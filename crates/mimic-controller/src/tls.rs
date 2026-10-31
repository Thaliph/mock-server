//! HTTPS listener of the webhook (spec §7.1): the serving certificate lives in a slot that can
//! be swapped without restarting the listener.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper::service::{Service, service_fn};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::PoisonError;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;

/// The serving certificate, empty until one is loaded.
#[derive(Default, Debug)]
pub struct CertSlot {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl CertSlot {
    pub fn new() -> CertSlot {
        CertSlot::default()
    }

    /// Loads (or replaces) the certificate chain and private key, both PEM.
    pub fn set_pem(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<(), String> {
        let chain = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("certificate: {e}"))?;
        if chain.is_empty() {
            return Err("certificate: no certificate found".into());
        }
        let key =
            PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("private key: {e}"))?;
        let certified = CertifiedKey::from_der(chain, key, &provider())
            .map_err(|e| format!("certificate and key: {e}"))?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(certified));
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        self.get().is_some()
    }

    fn get(&self) -> Option<Arc<CertifiedKey>> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ResolvesServerCert for CertSlot {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.get()
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// How long a keep-alive connection may wait for its next request.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds on what the listener lets clients hold (spec §7.1, INJ-19).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Connections served at once.
    pub max_connections: usize,
    /// TLS handshake, then each request (headers and body).
    pub request_timeout: Duration,
    /// Keep-alive connection waiting for a next request.
    pub idle_timeout: Duration,
}

/// Serves `router` over TLS on `listener` until `stop` resolves, then lets in-flight requests
/// finish for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    slot: Arc<CertSlot>,
    router: Router,
    limits: Limits,
    grace: Duration,
    stop: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(std::io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(slot);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        stop.await;
        tracing::info!(grace_ms = grace.as_millis() as u64, "shutdown started");
        let _ = stop_tx.send(true);
    });
    let mut connections = JoinSet::new();
    let mut stopped = stop_rx.clone();
    // A permit is taken before accepting: further clients wait in the accept backlog.
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    loop {
        let permit = tokio::select! {
            Ok(permit) = slots.clone().acquire_owned() => permit,
            _ = stopped.wait_for(|stop| *stop) => break,
        };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stopped.wait_for(|stop| *stop) => break,
        };
        let (stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "webhook accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let (acceptor, router, stop) = (acceptor.clone(), router.clone(), stop_rx.clone());
        connections.spawn(async move {
            connection(stream, acceptor, router, limits, stop).await;
            drop(permit);
        });
    }
    drop(listener);
    let drained = tokio::time::timeout(grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!("grace period over, closing remaining webhook connections");
    }
    tracing::info!("shutdown complete");
    Ok(())
}

/// Where a connection is in its request cycle, for the deadlines of [`Limits`].
#[derive(Clone, Copy)]
enum Phase {
    /// Waiting for the first byte of a request since this instant.
    Idle(Instant),
    /// A request started (first byte read) at this instant and is not answered yet.
    Request(Instant),
}

#[derive(Default)]
struct Cycle {
    phase: Mutex<Option<Phase>>,
    changed: Notify,
}

impl Cycle {
    fn set(&self, phase: Phase) {
        *self.phase.lock().unwrap_or_else(PoisonError::into_inner) = Some(phase);
        self.changed.notify_one();
    }

    fn get(&self) -> Option<Phase> {
        *self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves when the connection overstays the deadline of its phase.
    async fn expired(&self, limits: Limits) {
        loop {
            let Some(phase) = self.get() else {
                return std::future::pending().await;
            };
            let deadline = match phase {
                Phase::Idle(since) => since + limits.idle_timeout,
                Phase::Request(since) => since + limits.request_timeout,
            };
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    // The phase may have changed while sleeping: only the same one expires.
                    let unchanged = match (phase, self.get()) {
                        (Phase::Idle(a), Some(Phase::Idle(b))) => a == b,
                        (Phase::Request(a), Some(Phase::Request(b))) => a == b,
                        _ => false,
                    };
                    if unchanged {
                        return;
                    }
                }
                () = self.changed.notified() => {}
            }
        }
    }
}

/// The TLS stream, noting when the first byte of a request arrives.
struct Watched<S> {
    inner: S,
    cycle: Arc<Cycle>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        let started = matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before;
        if started && matches!(self.cycle.get(), Some(Phase::Idle(_))) {
            self.cycle.set(Phase::Request(Instant::now()));
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    router: Router,
    limits: Limits,
    mut stop: watch::Receiver<bool>,
) {
    let handshake = tokio::time::timeout(limits.request_timeout, acceptor.accept(stream));
    let tls = match handshake.await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "webhook TLS handshake failed");
            return;
        }
        Err(_) => return,
    };
    let cycle = Arc::new(Cycle::default());
    cycle.set(Phase::Idle(Instant::now()));
    let inner = TowerToHyperService::new(router);
    let service = service_fn({
        let cycle = cycle.clone();
        move |request| {
            let (answer, cycle) = (inner.call(request), cycle.clone());
            async move {
                let response = answer.await;
                cycle.set(Phase::Idle(Instant::now()));
                response
            }
        }
    });
    let io = Watched {
        inner: tls,
        cycle: cycle.clone(),
    };
    let connection = http1::Builder::new().serve_connection(TokioIo::new(io), service);
    let mut connection = std::pin::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(e) = result {
                tracing::debug!(error = %e, "webhook connection error");
            }
        }
        () = cycle.expired(limits) => {
            tracing::debug!("webhook connection closed: deadline passed");
        }
        () = async { let _ = stop.wait_for(|stop| *stop).await; } => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certs::{self, Decision, Stored};
    use axum::routing::get;
    use std::time::{Instant, SystemTime};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_rustls::client::TlsStream;

    const TIMEOUT: Duration = Duration::from_millis(500);
    const NAME: &str = "mimic.mimic-system.svc";

    struct Listener {
        addr: std::net::SocketAddr,
        connector: TlsConnector,
    }

    async fn start(max_connections: usize, idle_timeout: Duration) -> Listener {
        let stored =
            certs::issue(&Stored::default(), Decision::NewCa, NAME, SystemTime::now()).unwrap();
        let slot = Arc::new(CertSlot::new());
        slot.set_pem(&stored.tls_crt, &stored.tls_key).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for ca in CertificateDer::pem_slice_iter(&stored.ca_crt) {
            roots.add(ca.unwrap()).unwrap();
        }
        let client = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route("/ok", get(|| async { "ok" }));
        let limits = Limits {
            max_connections,
            request_timeout: TIMEOUT,
            idle_timeout,
        };
        tokio::spawn(serve(
            listener,
            slot,
            router,
            limits,
            Duration::from_secs(1),
            std::future::pending(),
        ));
        Listener {
            addr,
            connector: TlsConnector::from(Arc::new(client)),
        }
    }

    impl Listener {
        async fn tls(&self) -> TlsStream<TcpStream> {
            let tcp = TcpStream::connect(self.addr).await.unwrap();
            let name = NAME.to_string().try_into().unwrap();
            self.connector.connect(name, tcp).await.unwrap()
        }
    }

    /// True once the peer closes the connection (EOF or reset) within `within`.
    async fn closed_within(stream: &mut (impl AsyncReadExt + Unpin), within: Duration) -> bool {
        let mut buffer = [0; 256];
        let end = Instant::now() + within;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, stream.read(&mut buffer)).await {
                Err(_) => return false,
                Ok(Ok(0) | Err(_)) => return true,
                Ok(Ok(_)) => {}
            }
        }
    }

    const MARGIN: Duration = Duration::from_secs(1);

    async fn get_ok(stream: &mut TlsStream<TcpStream>) -> String {
        stream
            .write_all(b"GET /ok HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buffer = [0; 512];
        let n = stream.read(&mut buffer).await.unwrap();
        String::from_utf8_lossy(&buffer[..n]).into_owned()
    }

    #[tokio::test]
    async fn inj_19_a_client_that_never_starts_tls_is_closed_within_the_timeout() {
        let server = start(4, Duration::from_secs(60)).await;
        let mut tcp = TcpStream::connect(server.addr).await.unwrap();
        assert!(closed_within(&mut tcp, TIMEOUT + MARGIN).await);
    }

    #[tokio::test]
    async fn inj_19_tls_then_silence_is_closed_within_the_request_timeout() {
        let server = start(4, Duration::from_secs(60)).await;
        let mut tls = server.tls().await;
        let started = Instant::now();
        assert!(closed_within(&mut tls, TIMEOUT + MARGIN).await);
        assert!(started.elapsed() >= TIMEOUT - Duration::from_millis(100));
    }

    #[tokio::test]
    async fn inj_19_headers_sent_byte_by_byte_are_closed_within_the_timeout() {
        let server = start(4, Duration::from_secs(60)).await;
        let mut tls = server.tls().await;
        let started = Instant::now();
        for byte in b"GET /ok HTTP/1.1\r\nhost: x\r\nx-slow: ".iter().cycle() {
            if tls.write_all(&[*byte]).await.is_err() || started.elapsed() > TIMEOUT + MARGIN {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(closed_within(&mut tls, Duration::from_millis(200)).await);
        assert!(
            started.elapsed() < TIMEOUT + MARGIN,
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn inj_19_a_review_waits_for_a_free_slot_and_is_answered_within_twice_the_timeout() {
        let server = start(4, Duration::from_secs(60)).await;
        let mut stalled = Vec::new();
        for _ in 0..4 {
            stalled.push(TcpStream::connect(server.addr).await.unwrap());
        }
        let started = Instant::now();
        let mut tls = server.tls().await;
        assert!(get_ok(&mut tls).await.starts_with("HTTP/1.1 200"));
        assert!(started.elapsed() >= TIMEOUT / 2, "{:?}", started.elapsed());
        assert!(started.elapsed() < 2 * TIMEOUT + Duration::from_millis(500));
    }

    #[tokio::test]
    async fn inj_19_an_idle_keep_alive_connection_is_closed_after_the_idle_duration() {
        let idle = Duration::from_millis(1200);
        let server = start(4, idle).await;
        let mut tls = server.tls().await;
        assert!(get_ok(&mut tls).await.starts_with("HTTP/1.1 200"));
        let started = Instant::now();
        // Longer than the request timeout, shorter than the idle duration.
        assert!(!closed_within(&mut tls, TIMEOUT + Duration::from_millis(300)).await);
        assert!(closed_within(&mut tls, idle).await);
        assert!(started.elapsed() >= idle - Duration::from_millis(100));
    }
}
