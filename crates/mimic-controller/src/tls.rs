//! HTTPS listener of the webhook (spec §7.1): the serving certificate lives in a slot that can
//! be swapped without restarting the listener.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::PoisonError;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The serving certificate, empty until one is loaded.
#[derive(Default, Debug)]
pub struct CertSlot {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl CertSlot {
    pub fn new() -> CertSlot {
        CertSlot::default()
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "called by the certificate manager")
    )]
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

    #[cfg_attr(not(test), expect(dead_code, reason = "read by the readiness check"))]
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

/// Serves `router` over TLS on `listener` until `stop` resolves, then lets in-flight requests
/// finish for at most `grace`.
pub async fn serve(
    listener: TcpListener,
    slot: Arc<CertSlot>,
    router: Router,
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
    loop {
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
        connections.spawn(connection(
            stream,
            acceptor.clone(),
            router.clone(),
            stop_rx.clone(),
        ));
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

async fn connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    router: Router,
    mut stop: watch::Receiver<bool>,
) {
    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "webhook TLS handshake failed");
            return;
        }
        Err(_) => return,
    };
    let service = TowerToHyperService::new(router);
    let connection = http1::Builder::new().serve_connection(TokioIo::new(tls), service);
    let mut connection = std::pin::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(e) = result {
                tracing::debug!(error = %e, "webhook connection error");
            }
        }
        () = async { let _ = stop.wait_for(|stop| *stop).await; } => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}
