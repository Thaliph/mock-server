//! Plan de contrôle de mimic (spec §5.5) : agrège les enregistrements des sidecars par
//! workload, sert l'interface de découverte, construit les snapshots et porte l'état souhaité
//! lu dans les ressources `MimicWorkload`. Seule autorité de bascule des sidecars.

pub mod aggregated;
pub mod api;
pub mod auth;
pub mod config;
pub mod crd;
pub mod kube;
pub mod leader;
pub mod metrics;
pub mod mock_form;
pub mod reconcile;
pub mod state;
pub mod ui;

use std::net::SocketAddr;
use std::sync::Arc;

use mimic_net::serve::{ServeOptions, serve};
use mimic_net::tls::Tls;
use object_store::ObjectStore;
use tokio::net::TcpListener;

use crate::aggregated::{Authorizer, KubeAuthorizer};
use crate::auth::{Authenticator, KubeAuth};
use crate::config::{AuthConfig, ControllerConfig};
use crate::leader::{Coordination, Election, Role};
use crate::reconcile::MimicWorkloadSpec;
use crate::state::Ctl;

/// Rétention, compaction et oubli des pods (spec §5.5.2).
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub struct Running {
    ctl: Arc<Ctl>,
    api: SocketAddr,
    ui: SocketAddr,
    /// API agrégée de lecture (spec §9.2c), si configurée.
    aggregated: Option<SocketAddr>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Élection de la réplique active ; absente : une seule réplique, toujours active.
    election: Option<Arc<Election>>,
}

impl Running {
    pub fn api_addr(&self) -> SocketAddr {
        self.api
    }

    pub fn ui_addr(&self) -> SocketAddr {
        self.ui
    }

    pub fn aggregated_addr(&self) -> Option<SocketAddr> {
        self.aggregated
    }

    pub fn state(&self) -> &Arc<Ctl> {
        &self.ctl
    }

    /// Équivalent d'une création, modification (`Some`) ou suppression (`None`) de
    /// la ressource `MimicWorkload` : utilisé par les tests et hors cluster.
    pub async fn apply_spec(&self, ns: &str, wl: &str, spec: Option<MimicWorkloadSpec>) {
        self.ctl.apply_spec(ns, wl, spec, None).await;
    }

    /// Équivalent d'une création ou modification du namespace `ns` avec ces labels
    /// (garde-fou production, spec §5.5.1) : utilisé par les tests et hors cluster.
    pub async fn set_namespace_labels(
        &self,
        ns: &str,
        labels: &std::collections::BTreeMap<String, String>,
    ) {
        self.ctl.set_namespace_labels(ns, labels).await;
    }

    /// Vrai si cette réplique sert les sidecars (spec §5.5.4).
    pub fn is_leader(&self) -> bool {
        self.ctl.is_active()
    }

    /// Attend que la réplique soit active, état rechargé et marque posée ; immédiat sans
    /// élection.
    pub async fn leading(&self) {
        if let Some(e) = &self.election {
            e.wait(Role::Active).await;
        }
    }

    /// Attend la perte du bail : la réplique ne sert plus et le processus doit s'arrêter.
    /// Ne se termine jamais sans élection.
    pub async fn lost(&self) {
        match &self.election {
            Some(e) => e.wait(Role::Lost).await,
            None => std::future::pending().await,
        }
    }

    /// Arrêt propre. Sans élection, arrête les tâches après avoir écrit les exemples par
    /// endpoint qui ont changé ; avec, retire d'abord la marque et cesse de servir, puis
    /// écrit les exemples et rend le bail (spec §5.5.4).
    pub async fn shutdown(self) {
        match &self.election {
            Some(e) => e.shutdown(&self.ctl).await,
            None => self.ctl.flush_all().await,
        }
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
    }

    /// Arrêt brutal, comme un processus tué (tests) : rien n'est écrit ni rendu.
    pub fn kill(self) {
        self.ctl.set_active(false);
        if let Some(e) = &self.election {
            e.kill();
        }
        for t in &self.tasks {
            t.abort();
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
    config.validate()?;
    let store = open(&config.store)?;
    start_with(config, store).await
}

/// Comme `start`, sur un support déjà ouvert (tests). Avec `leader_election`, sur le bail
/// Kubernetes, ce qui exige `auth: kubernetes`.
pub async fn start_with(
    config: ControllerConfig,
    store: Arc<dyn ObjectStore>,
) -> Result<Running, String> {
    launch(config, store, None, None).await
}

/// Comme `start`, avec l'API agrégée autorisée par `authorizer` au lieu de
/// `SubjectAccessReview` (tests, hors cluster) ; `aggregated.client_ca_file` est requis.
pub async fn start_authorized(
    config: ControllerConfig,
    authorizer: Arc<dyn Authorizer>,
) -> Result<Running, String> {
    config.validate()?;
    let store = open(&config.store)?;
    launch(config, store, None, Some(authorizer)).await
}

/// Réplique élue sur un bail donné (tests, hors cluster) ; `leader_election` est requis.
pub async fn start_elected(
    config: ControllerConfig,
    store: Arc<dyn ObjectStore>,
    coord: Arc<dyn Coordination>,
) -> Result<Running, String> {
    if config.leader_election.is_none() {
        return Err("start_elected exige leader_election".into());
    }
    launch(config, store, Some(coord), None).await
}

async fn launch(
    config: ControllerConfig,
    store: Arc<dyn ObjectStore>,
    coord: Option<Arc<dyn Coordination>>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> Result<Running, String> {
    install_crypto_provider();
    config.validate()?;
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
        Some(files) => Some(Tls::new(files, &[b"h2", b"http/1.1"])?),
        None => None,
    };
    // API agrégée : CA du front proxy et autorisation, avant d'écouter (spec §9.2c).
    let aggregated = match (&config.aggregated, &config.tls) {
        (Some(agg), Some(files)) => {
            let (ca, names) = match &agg.client_ca_file {
                Some(path) => (
                    std::fs::read_to_string(path).map_err(|e| {
                        format!("aggregated.client_ca_file {}: {e}", path.display())
                    })?,
                    agg.allowed_names.clone(),
                ),
                None => {
                    let client = client
                        .as_ref()
                        .ok_or("aggregated sans client_ca_file exige auth kubernetes")?;
                    let (ca, names) = aggregated::front_proxy_ca(client).await?;
                    let names = if agg.allowed_names.is_empty() {
                        names
                    } else {
                        agg.allowed_names.clone()
                    };
                    (ca, names)
                }
            };
            let authorizer = match (authorizer, &client) {
                (Some(a), _) => a,
                (None, Some(client)) => Arc::new(KubeAuthorizer::new(client.clone())),
                (None, None) => return Err("aggregated exige auth kubernetes".into()),
            };
            let tls = Tls::with_client_ca(files, &[b"h2", b"http/1.1"], ca.as_bytes(), names)?;
            Some((agg.listen, tls, authorizer))
        }
        _ => None,
    };
    let election = match &config.leader_election {
        None => None,
        Some(le) => {
            let coord = match (coord, &client) {
                (Some(c), _) => c,
                (None, Some(client)) => Arc::new(crate::kube::KubeLease::new(
                    client.clone(),
                    &le.namespace,
                    &le.lease,
                )) as Arc<dyn Coordination>,
                (None, None) => {
                    return Err("leader_election exige auth kubernetes (bail Kubernetes)".into());
                }
            };
            Some(Election::new(coord, le.identity()?, le.timings()))
        }
    };
    let ctl = Arc::new(Ctl::new(config, store, auth));
    if let Some(t) = &tls {
        ctl.metrics.watch_certificate(t.reloader().clone());
    }
    // Garde-fou production : les labels des namespaces sont connus avant de servir un
    // sidecar ou de réconcilier une ressource (spec §5.5.1). Avec élection, la prise de
    // relais s'en charge.
    if election.is_none() {
        if let Some(client) = &client {
            crate::kube::load_namespaces(&ctl, client).await?;
        }
        ctl.set_active(true);
    }

    let api_listener = TcpListener::bind(ctl.config.listen)
        .await
        .map_err(|e| format!("écoute sur {}: {e}", ctl.config.listen))?;
    let ui_listener = TcpListener::bind(ctl.config.ui_listen)
        .await
        .map_err(|e| format!("écoute sur {}: {e}", ctl.config.ui_listen))?;
    let api = api_listener.local_addr().map_err(|e| e.to_string())?;
    let ui = ui_listener.local_addr().map_err(|e| e.to_string())?;
    let aggregated = match aggregated {
        Some((listen, tls, authorizer)) => {
            let listener = TcpListener::bind(listen)
                .await
                .map_err(|e| format!("écoute sur {listen}: {e}"))?;
            Some((listener, tls, authorizer))
        }
        None => None,
    };
    let aggregated_addr = match &aggregated {
        Some((l, _, _)) => Some(l.local_addr().map_err(|e| e.to_string())?),
        None => None,
    };
    // Les serveurs s'arrêtent avec leurs tâches (`shutdown`, `kill`).
    let mut tasks = vec![
        tokio::spawn(serve(
            api_listener,
            api::router(ctl.clone()),
            tls,
            ServeOptions::default(),
            std::future::pending(),
        )),
        tokio::spawn(serve(
            ui_listener,
            ui::router(ctl.clone()),
            None,
            ServeOptions::default(),
            std::future::pending(),
        )),
    ];
    if let Some((listener, tls, authorizer)) = aggregated {
        tasks.push(tokio::spawn(serve(
            listener,
            aggregated::router(ctl.clone(), authorizer),
            Some(tls),
            ServeOptions::default(),
            std::future::pending(),
        )));
    }
    match &election {
        // La passive n'a ni maintenance ni surveillance : la prise de relais les lance.
        Some(e) => e.spawn(ctl.clone(), client),
        None => {
            tasks.push(tokio::spawn(maintenance(ctl.clone())));
            if let Some(client) = client {
                tasks.push(tokio::spawn(crate::kube::watch_namespaces(
                    ctl.clone(),
                    client.clone(),
                )));
                tasks.push(tokio::spawn(crate::kube::watch(
                    ctl.clone(),
                    client.clone(),
                    None,
                )));
                tasks.push(tokio::spawn(crate::kube::publish_status(
                    ctl.clone(),
                    client,
                )));
            }
        }
    }
    tracing::info!(%api, %ui, tls = ctl.config.tls.is_some(), election = election.is_some(), "contrôleur démarré");
    Ok(Running {
        ctl,
        api,
        ui,
        aggregated: aggregated_addr,
        tasks,
        election,
    })
}

/// Passe de maintenance du support et des pods (spec §5.5.2), après chaque intervalle.
pub(crate) async fn maintenance(ctl: Arc<Ctl>) {
    loop {
        tokio::time::sleep(MAINTENANCE_INTERVAL).await;
        ctl.maintain().await;
    }
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
