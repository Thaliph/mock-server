//! Sidecar mimic : proxy d'observation et de mock du trafic HTTP d'un pod.
//!
//! Voir `docs/SPEC.md`. La logique métier vit dans `mimic-core` ; ce crate l'adapte au réseau.

pub mod admin;
pub mod capture;
pub mod config;
pub mod metrics;
pub mod net;
pub mod proxy;
pub mod share;
pub mod state;
pub mod store;
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
enum Listener {
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
        share::final_sync(&self.state).await;
        for t in self.tasks {
            t.abort();
        }
    }
}

/// Démarre les listeners et les tâches de fond.
pub async fn start(config: Config) -> Result<Running, String> {
    config.validate()?;
    let state = AppState::new(config);
    if let Some(dir) = &state.config.store.dir {
        let loaded = state
            .store
            .persist_to(dir)
            .map_err(|e| format!("store {}: {e}", dir.display()))?;
        tracing::info!(loaded, dir = %dir.display(), "store local rechargé");
    }
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
    if let Some(path) = state.config.annotations_file.clone() {
        tasks.push(tokio::spawn(watch::watch_annotations(state.clone(), path)));
    }
    if let Some(t) = share::spawn(&state)? {
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
        tasks,
    })
}

async fn serve(listener: TcpListener, state: Arc<AppState>, kind: Listener) {
    let listen = match listener.local_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "accept échoué");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let info = ConnInfo {
            listen,
            peer,
            original_dst: net::original_dst(&stream),
        };
        let state = state.clone();
        tokio::spawn(async move {
            // Le trafic redirigé par iptables n'est pas forcément HTTP.
            if let (Some(dst), Listener::Inbound | Listener::Outbound) = (info.original_dst, kind) {
                let protocol = net::sniff(&stream).await;
                if protocol != net::Protocol::Http {
                    let inbound = matches!(kind, Listener::Inbound);
                    proxy::tunnel(state, stream, dst, protocol, inbound).await;
                    return;
                }
            }
            let svc = service_fn(move |req| {
                let state = state.clone();
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
                .serve_connection_with_upgrades(TokioIo::new(stream), svc)
                .await;
        });
    }
}
