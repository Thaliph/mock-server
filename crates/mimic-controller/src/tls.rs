//! HTTPS listener of the webhook (spec §7.1): the serving certificate lives in a slot that can
//! be swapped without restarting the listener.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::TcpListener;

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
        let _ = (cert_pem, key_pem, &self.current);
        todo!()
    }

    pub fn is_loaded(&self) -> bool {
        todo!()
    }
}

impl ResolvesServerCert for CertSlot {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        todo!()
    }
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
    let _ = (listener, slot, router, grace, stop);
    todo!()
}
