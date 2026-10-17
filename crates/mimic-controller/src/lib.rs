//! Plan de contrôle de mimic (spec §5.5) : agrège les enregistrements des sidecars par
//! workload, sert l'interface de découverte, construit les snapshots et porte l'état souhaité
//! lu dans les ressources `MimicWorkload`. Seule autorité de bascule des sidecars.

pub mod api;
pub mod auth;
pub mod config;
pub mod crd;
pub mod kube;
pub mod metrics;
pub mod reconcile;
pub mod state;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::service::TowerToHyperService;
use object_store::ObjectStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;

use crate::auth::{Authenticator, KubeAuth};
use crate::config::{AuthConfig, ControllerConfig, TlsFiles};
use crate::reconcile::MimicWorkloadSpec;
use crate::state::Ctl;

pub struct Running {
    ctl: Arc<Ctl>,
    api: SocketAddr,
    ui: SocketAddr,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Running {
    pub fn api_addr(&self) -> SocketAddr {
        self.api
    }

    pub fn ui_addr(&self) -> SocketAddr {
        self.ui
    }

    pub fn state(&self) -> &Arc<Ctl> {
        &self.ctl
    }

    /// Équivalent d'une création, modification (`Some`) ou suppression (`None`) de
    /// la ressource `MimicWorkload` : utilisé par les tests et hors cluster.
    pub async fn apply_spec(&self, ns: &str, wl: &str, spec: Option<MimicWorkloadSpec>) {
        self.ctl.apply_spec(ns, wl, spec, None).await;
    }

    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
    }

    /// Attend la fin des tâches (arrêt par signal).
    pub async fn wait(self) {
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

/// Choisit ring comme fournisseur cryptographique du processus. Deux fournisseurs sont
/// compilés (aws-lc-rs vient d'object_store) : sans ce choix, le premier client TLS
/// construit par `kube` panique. Sans effet si un fournisseur est déjà installé.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub async fn start(config: ControllerConfig) -> Result<Running, String> {
    install_crypto_provider();
    config.validate()?;
    let store = open(&config.store)?;
    let (auth, client) = match &config.auth {
        AuthConfig::Static { tokens } => (Authenticator::Static(tokens.clone()), None),
        AuthConfig::Kubernetes => {
            let client = ::kube::Client::try_default()
                .await
                .map_err(|e| format!("client Kubernetes: {e}"))?;
            (
                Authenticator::Kubernetes(KubeAuth::new(client.clone())),
                Some(client),
            )
        }
    };
    let tls = match &config.tls {
        Some(files) => Some(tls_acceptor(files)?),
        None => None,
    };
    let ctl = Arc::new(Ctl::new(config, store, auth));

    let api_listener = TcpListener::bind(ctl.config.listen)
        .await
        .map_err(|e| format!("écoute sur {}: {e}", ctl.config.listen))?;
    let ui_listener = TcpListener::bind(ctl.config.ui_listen)
        .await
        .map_err(|e| format!("écoute sur {}: {e}", ctl.config.ui_listen))?;
    let api = api_listener.local_addr().map_err(|e| e.to_string())?;
    let ui = ui_listener.local_addr().map_err(|e| e.to_string())?;
    let mut tasks = vec![
        tokio::spawn(serve(api_listener, api::router(ctl.clone()), tls)),
        tokio::spawn(serve(ui_listener, ui::router(ctl.clone()), None)),
    ];
    if let Some(client) = client {
        tasks.push(tokio::spawn(crate::kube::watch(
            ctl.clone(),
            client.clone(),
        )));
        tasks.push(tokio::spawn(crate::kube::publish_status(
            ctl.clone(),
            client,
        )));
    }
    tracing::info!(%api, %ui, tls = ctl.config.tls.is_some(), "contrôleur démarré");
    Ok(Running {
        ctl,
        api,
        ui,
        tasks,
    })
}

async fn serve(listener: TcpListener, router: Router, tls: Option<tokio_rustls::TlsAcceptor>) {
    // Les connexions appartiennent à cette tâche : l'arrêter les ferme aussi.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(router.clone());
        let tls = tls.clone();
        connections.spawn(async move {
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let result = match tls {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(s) => builder.serve_connection(TokioIo::new(s), service).await,
                    Err(e) => {
                        tracing::debug!(error = %e, "poignée de main TLS refusée");
                        return;
                    }
                },
                None => {
                    builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                }
            };
            if let Err(e) = result {
                tracing::debug!(error = %e, "connexion terminée");
            }
        });
    }
}

fn tls_acceptor(files: &TlsFiles) -> Result<tokio_rustls::TlsAcceptor, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&files.cert_file)
        .and_then(|it| it.collect())
        .map_err(|e| format!("certificat {}: {e}", files.cert_file.display()))?;
    let key = PrivateKeyDer::from_pem_file(&files.key_file)
        .map_err(|e| format!("clé {}: {e}", files.key_file.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("certificat du contrôleur: {e}"))?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server)))
}

fn open(url: &str) -> Result<Arc<dyn ObjectStore>, String> {
    if let Some(dir) = url.strip_prefix("file://") {
        std::fs::create_dir_all(dir).map_err(|e| format!("répertoire {dir}: {e}"))?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(dir)
            .map_err(|e| format!("répertoire {dir}: {e}"))?;
        return Ok(Arc::new(fs));
    }
    if let Some(rest) = url.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        // Identifiants, région et endpoint lus dans l'environnement (AWS_*), IRSA compris.
        let s3 = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .map_err(|e| format!("bucket {bucket}: {e}"))?;
        let store: Arc<dyn ObjectStore> = Arc::new(s3);
        let prefix = prefix.trim_matches('/');
        return Ok(if prefix.is_empty() {
            store
        } else {
            Arc::new(object_store::prefix::PrefixStore::new(store, prefix))
        });
    }
    Err(format!("store non supporté: {url}"))
}
