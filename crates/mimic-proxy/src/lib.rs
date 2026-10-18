//! Sidecar mimic : proxy d'observation et de mock du trafic HTTP d'un pod.
//!
//! Voir `docs/SPEC.md`. La logique métier vit dans `mimic-core` ; ce crate l'adapte au réseau.

pub mod admin;
pub mod capture;
pub mod config;
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

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
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
pub async fn start(config: Config) -> Result<Running, String> {
    config.validate()?;
    let state = AppState::new(config)?;
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
    // Un sidecar piloté n'obéit qu'au contrôleur : l'annotation de mode est ignorée.
    let controller = match &state.config.share {
        Some(share) if share.is_controller() => {
            control::restore(&state);
            let client = Arc::new(control::ControllerClient::new(&state, share)?);
            tasks.push(control::spawn(
                state.clone(),
                client.clone(),
                share.mode_poll,
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
        let _ = stream.set_nodelay(true);
        let info = ConnInfo {
            listen,
            peer,
            original_dst: net::original_dst(&stream),
            tls_authority: None,
        };
        let state = state.clone();
        tokio::spawn(async move {
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
    match acceptor.accept(io).await {
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
                    reason: "client_rejected".into(),
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
    let svc = service_fn(move |req| {
        let state = state.clone();
        let info = info.clone();
        async move {
            let resp = match kind {
                Listener::Inbound => proxy::inbound(state, info, req).await,
                Listener::Outbound => proxy::outbound(state, info, req).await,
                Listener::Admin => admin::handle(state, req).await,
            };
            Ok::<_, Infallible>(resp)
        }
    });
    let _ = auto::Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(io, svc)
        .await;
}
