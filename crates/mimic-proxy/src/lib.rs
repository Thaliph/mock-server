//! Sidecar mimic : proxy d'observation et de mock du trafic HTTP d'un pod.
//!
//! Voir `docs/SPEC.md`. La logique métier vit dans `mimic-core` ; ce crate l'adapte au réseau.

pub mod admin;
pub mod capture;
pub mod config;
pub mod connect;
pub mod control;
pub mod engine;
pub mod journal;
pub mod metrics;
pub mod net;
pub mod proxy;
pub mod share;
pub mod state;
pub mod store;
pub mod throttle;
pub mod tls;
pub mod watch;

mod probe;
pub use probe::{probe, probe_addr};

/// Parseurs internes ouverts aux cibles de `fuzz/`, compilées avec `--cfg fuzzing` : absents
/// de toute autre compilation, l'API publique du crate n'en change pas.
#[cfg(fuzzing)]
pub mod fuzzing {
    /// Voir `capture::decode`.
    pub fn decode(encoding: &str, data: &[u8], limit: usize) -> Option<Vec<u8>> {
        crate::capture::decode(encoding, data, limit)
    }
}

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::proxy::ConnInfo;
use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Listener {
    Inbound,
    Outbound,
    Admin,
}

impl Listener {
    fn as_str(self) -> &'static str {
        match self {
            Listener::Inbound => "inbound",
            Listener::Outbound => "outbound",
            Listener::Admin => "admin",
        }
    }
}

/// Sidecar démarré.
pub struct Running {
    state: Arc<AppState>,
    inbound: Option<SocketAddr>,
    outbound: Option<SocketAddr>,
    admin: SocketAddr,
    controller: Option<Arc<control::ControllerClient>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound.expect("listener entrant désactivé")
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound.expect("listener sortant désactivé")
    }

    pub fn admin_addr(&self) -> SocketAddr {
        self.admin
    }

    /// Arrête les listeners et les tâches de fond, après une dernière synchronisation.
    pub async fn shutdown(self) {
        share::final_sync(&self.state, self.controller.clone()).await;
        for t in self.tasks {
            t.abort();
        }
        self.state.store.sync_journal();
    }
}

/// Démarre les listeners et les tâches de fond.
pub async fn start(mut config: Config) -> Result<Running, String> {
    config.local_mode();
    config.validate()?;
    let state = AppState::new(config)?;
    // Mock local (spec §9.7) : le snapshot du fichier, relu et vérifié comme un import.
    if let Some(path) = &state.config.mock.from {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let snapshot = mimic_core::snapshot::Snapshot::parse(&bytes)
            .map_err(|e| format!("snapshot {}: {e}", path.display()))?;
        tracing::info!(file = %path.display(), snapshot = %snapshot.id, exchanges = snapshot.exchanges.len(), "mock local");
        state.set_snapshot(Some(std::sync::Arc::new(
            mimic_core::snapshot::MockSnapshot::from_snapshot(&snapshot),
        )));
    }
    if let Some(dir) = &state.config.store.dir {
        let journal = journal::JournalConfig::new(
            dir,
            state.config.store.segment_bytes,
            state.config.store.max_segments,
        );
        let loaded = state
            .store
            .persist_to(&journal)
            .map_err(|e| format!("store {}: {e}", dir.display()))?;
        tracing::info!(loaded, dir = %dir.display(), "store local rechargé");
    }
    // Sous contrôleur, le dernier état souhaité gardé (ou l'attente en mock) s'applique avant
    // la première connexion : rien de ce qui arrive ne sort s'il s'agit d'un mock (§5.5.5).
    if state.config.controlled() {
        control::prepare_startup(&state).await;
    }
    // Démarrage en mock : moteur construit avant d'écouter (spec §5.3).
    state.ensure_mock_engine();
    let mut tasks = vec![];
    let bind = |addr: SocketAddr, kind: Listener| {
        let state = state.clone();
        async move {
            let listener = TcpListener::bind(addr)
                .await
                .map_err(|e| format!("écoute {kind:?} sur {addr}: {e}"))?;
            let local = listener.local_addr().map_err(|e| e.to_string())?;
            Ok::<_, String>((local, tokio::spawn(serve(listener, state, kind))))
        }
    };
    let inbound = if state.config.inbound.enabled {
        let (addr, t) = bind(state.config.inbound.listen, Listener::Inbound).await?;
        tasks.push(t);
        Some(addr)
    } else {
        None
    };
    let outbound = if state.config.outbound.enabled {
        let (addr, t) = bind(state.config.outbound.listen, Listener::Outbound).await?;
        tasks.push(t);
        Some(addr)
    } else {
        None
    };
    let (admin, t) = bind(state.config.admin.listen, Listener::Admin).await?;
    tasks.push(t);
    // `share.retention` reste accepté sans effet : c'est le contrôleur qui purge (spec §5.4).
    if let Some(retention) = state.config.share.as_ref().and_then(|s| s.retention) {
        tracing::warn!(
            retention_s = retention.as_secs(),
            "share.retention ignoré : la rétention est celle du contrôleur"
        );
    }
    // Un sidecar piloté n'obéit qu'au contrôleur : l'annotation de mode est ignorée.
    let controller = match &state.config.share {
        Some(share) if share.is_controller() => {
            let client = Arc::new(control::ControllerClient::new(&state, share)?);
            tasks.push(control::spawn(
                state.clone(),
                client.clone(),
                share.mode_poll,
            ));
            tasks.push(control::spawn_startup_deadline(
                state.clone(),
                share.startup.timeout,
            ));
            Some(client)
        }
        _ => {
            if let Some(path) = state.config.annotations_file.clone() {
                tasks.push(tokio::spawn(watch::watch_annotations(state.clone(), path)));
            }
            None
        }
    };
    if let Some(t) = share::spawn(&state, controller.clone())? {
        tasks.push(t);
    }
    tracing::info!(
        mode = state.mode().as_str(),
        ?inbound,
        ?outbound,
        %admin,
        "mimic démarré"
    );
    Ok(Running {
        state,
        inbound,
        outbound,
        admin,
        controller,
        tasks,
    })
}

async fn serve(listener: TcpListener, state: Arc<AppState>, kind: Listener) {
    let listen = match listener.local_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    // Erreur d'accept (EMFILE sous un tir) : pause et log limité plutôt qu'une boucle à vide.
    let errors = throttle::LogLimiter::per_second();
    loop {
        let (stream, peer) = throttle::accept_with_backoff(|| listener.accept(), &errors).await;
        // Plafond de connexions (spec §3.5) : au-delà, la connexion est refermée aussitôt,
        // sans être lue ; attendre une place pourrait bloquer une requête entrante qui
        // attend elle-même un appel sortant.
        let places = match kind {
            Listener::Admin => &state.admin_connections,
            Listener::Inbound | Listener::Outbound => &state.connections,
        };
        let Ok(place) = places.clone().try_acquire_owned() else {
            refuse(&state, kind, stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let info = ConnInfo {
            listen,
            peer,
            original_dst: net::original_dst(&stream),
            tls_authority: None,
        };
        let state = state.clone();
        tokio::spawn(async move {
            // Place rendue à la fin de la connexion, tunnels et WebSocket compris.
            let _place = place;
            // Le trafic redirigé par iptables n'est pas forcément HTTP.
            let mut stream = stream;
            if let (Some(dst), Listener::Inbound | Listener::Outbound) = (info.original_dst, kind) {
                let inbound = matches!(kind, Listener::Inbound);
                let (protocol, upstream) = match net::detect(&stream).await {
                    net::Detected::Protocol(p) => (p, None),
                    net::Detected::Silent => {
                        match proxy::resolve_silent(state.clone(), stream, dst, inbound).await {
                            proxy::Silent::Client(s, p, upstream) => {
                                stream = s;
                                (p, upstream)
                            }
                            proxy::Silent::Done => return,
                        }
                    }
                };
                if protocol == net::Protocol::Tls
                    && matches!(kind, Listener::Outbound)
                    && let Some(host) = net::peek_sni(&stream).await
                    && state.intercepts(&host)
                {
                    let authority = format!("{host}:{}", dst.port());
                    tls_serve(state, stream, info, authority).await;
                    return;
                }
                if protocol != net::Protocol::Http {
                    proxy::tunnel(state, stream, dst, protocol, inbound, upstream).await;
                    return;
                }
            }
            serve_http(TokioIo::new(stream), state, kind, info).await;
        });
    }
}

/// Connexion au-delà du plafond : refermée sans être lue, comptée.
fn refuse(state: &AppState, kind: Listener, stream: tokio::net::TcpStream) {
    drop(stream);
    state
        .metrics
        .connections_refused
        .get_or_create(&metrics::ListenerLabel {
            listener: kind.as_str(),
        })
        .inc();
    if let Some(omitted) = state.refused_log.allow(std::time::Instant::now()) {
        tracing::warn!(
            listener = kind.as_str(),
            omitted,
            "plafond de connexions simultanées atteint, connexion refusée"
        );
    }
}

/// Termine le TLS côté application avec un certificat forgé, puis traite le HTTP déchiffré.
pub(crate) async fn tls_serve<IO>(
    state: Arc<AppState>,
    io: IO,
    mut info: ConnInfo,
    authority: String,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Some(tls) = &state.tls else { return };
    let acceptor = tokio_rustls::TlsAcceptor::from(tls.server.clone());
    // Poignée de main bornée comme la lecture des en-têtes (spec §3.5).
    let handshake =
        tokio::time::timeout(state.config.server.header_read_timeout, acceptor.accept(io));
    let accepted = match handshake.await {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!(%authority, "poignée de main TLS interceptée hors délai");
            let label = state.metrics.authority(&authority);
            state
                .metrics
                .tls_handshake_failures
                .get_or_create(&metrics::TlsFailureLabels {
                    authority: label,
                    reason: "timeout",
                })
                .inc();
            return;
        }
    };
    match accepted {
        Ok(stream) => {
            info.tls_authority = Some(authority);
            serve_http(TokioIo::new(stream), state, Listener::Outbound, info).await;
        }
        Err(e) => {
            // Typiquement : l'application ne fait pas confiance à la CA, ou épingle le certificat.
            tracing::warn!(%authority, error = %e, "poignée de main TLS refusée par l'application");
            let label = state.metrics.authority(&authority);
            state
                .metrics
                .tls_handshake_failures
                .get_or_create(&metrics::TlsFailureLabels {
                    authority: label,
                    reason: "client_rejected",
                })
                .inc();
        }
    }
}

pub(crate) async fn serve_http<IO>(
    io: TokioIo<IO>,
    state: Arc<AppState>,
    kind: Listener,
    info: ConnInfo,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let header_read_timeout = state.config.server.header_read_timeout;
    let svc = service_fn(move |req| {
        let state = state.clone();
        let info = info.clone();
        async move {
            let resp = match kind {
                Listener::Inbound => proxy::inbound(state, info, req).await,
                Listener::Outbound => proxy::outbound(state, info, req).await,
                Listener::Admin => admin::handle(state, info.peer, req).await,
            };
            Ok::<_, Infallible>(resp)
        }
    });
    // Délai de lecture des en-têtes HTTP/1, inactivité keep-alive comprise (spec §3.5).
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let _ = builder.serve_connection_with_upgrades(io, svc).await;
}
