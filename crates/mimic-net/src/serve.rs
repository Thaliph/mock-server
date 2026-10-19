//! Boucle d'un serveur du plan de contrôle (spec §9.2b) : connexions bornées dans le temps,
//! pause après une erreur d'`accept`, relecture du certificat, arrêt propre.

use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::tls::Tls;

/// Pause après une erreur d'`accept` (EMFILE…) avant de réessayer (comme le sidecar, §9.3).
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Délais d'une connexion.
#[derive(Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Poignée de main TLS non terminée dans ce délai : connexion fermée.
    pub handshake_timeout: Duration,
    /// En-têtes HTTP/1 d'une requête non reçus dans ce délai : connexion fermée.
    pub header_read_timeout: Duration,
    /// À l'arrêt, attente des requêtes en cours avant de couper les connexions.
    pub drain_timeout: Duration,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            header_read_timeout: Duration::from_secs(10),
            drain_timeout: Duration::from_secs(10),
        }
    }
}

/// Au plus une ligne par période pour un même log ; les lignes omises sont comptées et
/// annoncées par la ligne suivante.
pub struct LogLimiter {
    period: Duration,
    /// Dernière ligne écrite, lignes omises depuis.
    state: Mutex<(Option<Instant>, u64)>,
}

impl LogLimiter {
    pub const fn new(period: Duration) -> Self {
        Self {
            period,
            state: Mutex::new((None, 0)),
        }
    }

    /// Une ligne par seconde.
    pub const fn per_second() -> Self {
        Self::new(Duration::from_secs(1))
    }

    /// `Some(omises)` si une ligne peut être écrite à `now`, avec le nombre de lignes omises
    /// depuis la précédente ; `None` si elle doit être omise (elle est alors comptée).
    pub fn allow(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (last, omitted) = &mut *state;
        if last.is_some_and(|t| now.saturating_duration_since(t) < self.period) {
            *omitted += 1;
            return None;
        }
        *last = Some(now);
        Some(std::mem::take(omitted))
    }
}

/// Attend la prochaine connexion. Après une erreur, l'écoute est suspendue
/// [`ACCEPT_BACKOFF`] puis reprise ; l'erreur est journalisée au plus une fois par seconde.
pub async fn accept_with_backoff<T, F, Fut>(mut accept: F, log: &LogLimiter) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    loop {
        match accept().await {
            Ok(c) => return c,
            Err(e) => {
                if let Some(omitted) = log.allow(Instant::now()) {
                    tracing::warn!(error = %e, omitted, "accept échoué, écoute suspendue 50 ms");
                }
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// Sert `router` sur `listener`, en HTTPS si `tls` est donné, jusqu'à `shutdown` : l'écoute
/// cesse, les requêtes en cours finissent (au plus `drain_timeout`), puis la fonction rend
/// la main. Les connexions appartiennent à la tâche : l'interrompre les ferme aussi.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Option<Tls>,
    opts: ServeOptions,
    shutdown: impl Future<Output = ()>,
) {
    let mut connections = tokio::task::JoinSet::new();
    let (stop, stopped) = watch::channel(false);
    let errors = LogLimiter::per_second();
    let period = tls.as_ref().map_or(Duration::from_secs(3600), |t| {
        t.reloader.files().reload_interval
    });
    let mut reload = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        let stream = tokio::select! {
            s = accept_with_backoff(|| listener.accept(), &errors) => s.0,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            // Fichiers d'un Secret monté : petits et en mémoire, lus sans quitter la tâche.
            _ = reload.tick(), if tls.is_some() => {
                if let Some(t) = &tls {
                    t.reloader.reload();
                }
                continue;
            }
            () = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(router.clone());
        let tls = tls.clone();
        let stopped = stopped.clone();
        connections.spawn(async move {
            match tls {
                Some(t) => {
                    match tokio::time::timeout(opts.handshake_timeout, t.acceptor.accept(stream))
                        .await
                    {
                        Ok(Ok(s)) => {
                            // Protocole négocié par ALPN : connu, il n'y a rien à deviner.
                            let proto = if s.get_ref().1.alpn_protocol() == Some(b"h2") {
                                Proto::Http2
                            } else {
                                Proto::Http1
                            };
                            connection(s, service, opts, stopped, proto).await;
                        }
                        Ok(Err(e)) => tracing::debug!(error = %e, "poignée de main TLS refusée"),
                        Err(_) => tracing::debug!("poignée de main TLS trop longue"),
                    }
                }
                None => {
                    // En clair, HTTP/1 ou HTTP/2 se devinent au premier octet : il doit
                    // arriver dans le délai des en-têtes.
                    let mut first = [0u8; 1];
                    match tokio::time::timeout(opts.header_read_timeout, stream.peek(&mut first))
                        .await
                    {
                        Ok(Ok(n)) if n > 0 => {
                            connection(stream, service, opts, stopped, Proto::Auto).await;
                        }
                        _ => tracing::debug!("connexion muette fermée"),
                    }
                }
            }
        });
    }
    drop(listener);
    let _ = stop.send(true);
    let drained = tokio::time::timeout(opts.drain_timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            remaining = connections.len(),
            "arrêt : connexions coupées après le délai de drainage"
        );
    }
}

/// Protocole d'une connexion.
enum Proto {
    /// Deviné aux premiers octets (connexion en clair).
    Auto,
    Http1,
    Http2,
}

/// Une connexion HTTP/1 ou HTTP/2 ; à l'arrêt, elle finit sa requête puis se ferme.
async fn connection<I>(
    io: I,
    service: TowerToHyperService<Router>,
    opts: ServeOptions,
    mut stopped: watch::Receiver<bool>,
    proto: Proto,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let builder = Builder::new(TokioExecutor::new());
    let mut builder = match proto {
        Proto::Auto => builder,
        Proto::Http1 => builder.http1_only(),
        Proto::Http2 => builder.http2_only(),
    };
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(opts.header_read_timeout);
    builder.http2().timer(TokioTimer::new());
    let conn = builder.serve_connection(TokioIo::new(io), service);
    tokio::pin!(conn);
    let result = tokio::select! {
        r = conn.as_mut() => r,
        // `Ref` du canal ne traverse pas un `await` : seule la fin de l'attente compte.
        () = async { let _ = stopped.wait_for(|s| *s).await; } => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, "connexion terminée");
    }
}
