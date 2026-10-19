//! Connexions vers l'amont bornées par `outbound.connect_timeout` (spec §3.5) : seul
//! l'établissement est borné, jamais une connexion établie (flux longs, WebSocket, tunnels).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use tokio::net::{TcpStream, ToSocketAddrs};

use crate::state::BoxError;

/// Connexion vers l'amont non établie dans le délai.
#[derive(Debug)]
pub struct ConnectTimeout(pub Duration);

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "connexion vers l'amont non établie en {:?} (outbound.connect_timeout)",
            self.0
        )
    }
}

impl std::error::Error for ConnectTimeout {}

/// Connecteur du client HTTP qui borne l'établissement complet d'une connexion : résolution
/// DNS, TCP et poignée de main TLS.
#[derive(Debug, Clone)]
pub struct TimeoutConnector<C> {
    inner: C,
    timeout: Duration,
}

impl<C> TimeoutConnector<C> {
    pub fn new(inner: C, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl<C> tower_service::Service<Uri> for TimeoutConnector<C>
where
    C: tower_service::Service<Uri>,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
    C::Response: Send + 'static,
{
    type Response = C::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<C::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let connecting = self.inner.call(dst);
        let timeout = self.timeout;
        Box::pin(async move {
            match tokio::time::timeout(timeout, connecting).await {
                Ok(r) => r.map_err(Into::into),
                Err(_) => Err(Box::new(ConnectTimeout(timeout)) as BoxError),
            }
        })
    }
}

/// Connexion TCP bornée par le délai ; au-delà, une erreur `TimedOut`.
pub async fn tcp(addr: impl ToSocketAddrs, timeout: Duration) -> std::io::Result<TcpStream> {
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            ConnectTimeout(timeout),
        )),
    }
}

/// Vrai si l'erreur, ou l'une de ses causes, est un délai de connexion dépassé.
pub fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(e) = cur {
        if e.is::<ConnectTimeout>()
            || e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        cur = e.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_found_among_the_causes() {
        let wrapped = std::io::Error::other(ConnectTimeout(Duration::from_secs(1)));
        assert!(is_timeout(&wrapped));
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(is_timeout(&timed_out));
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(!is_timeout(&refused));
    }
}
