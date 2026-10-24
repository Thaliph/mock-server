//! Configuration du contrôleur (spec §9.1).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

pub use mimic_net::tls::TlsFiles;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControllerConfig {
    /// API des sidecars (HTTPS si `tls` est renseigné).
    pub listen: SocketAddr,
    /// Interface en lecture seule ; liée à la boucle locale, joignable par port-forward.
    pub ui_listen: SocketAddr,
    pub tls: Option<TlsFiles>,
    pub auth: AuthConfig,
    /// `file:///chemin` ou `s3://bucket/prefixe`.
    pub store: String,
    #[serde(with = "duration_secs")]
    pub retention: Duration,
    pub max_exchanges_per_workload: usize,
    /// Échanges gardés en mémoire, tous workloads confondus.
    pub max_exchanges_total: usize,
    /// Bodies gardés par couple (opération, code), en mémoire et après compaction.
    pub samples_per_operation: usize,
    /// Volume des segments d'un workload sur le support : au-delà, les plus anciens partent.
    pub max_bytes_per_workload: u64,
    /// Endpoints gardés avec un exemple, par workload (spec §5.5.3).
    pub max_exemplars_per_workload: usize,
    /// Taille sérialisée des exemples d'un workload (spec §5.5.3).
    pub max_exemplar_bytes_per_workload: u64,
    /// Taille sérialisée des exemples de tous les workloads.
    pub max_exemplar_bytes_total: u64,
    /// Au plus un recalcul de la découverte d'un workload par intervalle.
    #[serde(with = "duration_secs")]
    pub discovery_interval: Duration,
    pub snapshot: SnapshotConfig,
    /// Un pod sans battement depuis ce délai est affiché absent.
    #[serde(with = "duration_secs")]
    pub pod_timeout: Duration,
    /// Un pod sans battement depuis ce délai est oublié.
    #[serde(with = "duration_secs")]
    pub pod_forget: Duration,
    /// Deux réplicas, une seule active (spec §5.5.4) ; absent : une seule réplique, sans bail.
    pub leader_election: Option<LeaderElection>,
    /// API agrégée de lecture `data.mimic.platform.io/v1alpha1` (spec §9.2c) ; absente,
    /// pas d'API agrégée.
    pub aggregated: Option<AggregatedConfig>,
}

/// API agrégée de lecture (spec §9.2c).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregatedConfig {
    pub listen: SocketAddr,
    /// CA des certificats clients du front proxy ; absente, `requestheader-client-ca-file`
    /// du ConfigMap `kube-system/extension-apiserver-authentication`.
    #[serde(default)]
    pub client_ca_file: Option<std::path::PathBuf>,
    /// Noms communs acceptés ; vide, ceux du ConfigMap (`requestheader-allowed-names`),
    /// ou tout nom si `client_ca_file` est donné.
    #[serde(default)]
    pub allowed_names: Vec<String>,
}

/// Élection de la réplique active par un bail `coordination.k8s.io/v1` (spec §5.5.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeaderElection {
    /// Nom du bail.
    pub lease: String,
    /// Namespace du bail et des pods du contrôleur.
    pub namespace: String,
    /// Identité de la réplique ; absente, `POD_NAME`, sinon `HOSTNAME`.
    pub identity: Option<String>,
    #[serde(with = "duration_secs")]
    pub lease_duration: Duration,
    #[serde(with = "duration_secs")]
    pub renew_deadline: Duration,
    #[serde(with = "duration_secs")]
    pub retry_period: Duration,
}

impl Default for LeaderElection {
    fn default() -> Self {
        let t = mimic_core::lease::Timings::default();
        Self {
            lease: "mimic-controller".into(),
            namespace: "mimic-system".into(),
            identity: None,
            lease_duration: Duration::from_millis(t.lease_duration_ms),
            renew_deadline: Duration::from_millis(t.renew_deadline_ms),
            retry_period: Duration::from_millis(t.retry_period_ms),
        }
    }
}

impl LeaderElection {
    pub fn timings(&self) -> mimic_core::lease::Timings {
        mimic_core::lease::Timings {
            lease_duration_ms: self.lease_duration.as_millis() as u64,
            renew_deadline_ms: self.renew_deadline.as_millis() as u64,
            retry_period_ms: self.retry_period.as_millis() as u64,
        }
    }

    /// Identité de la réplique : celle de la configuration, sinon le nom du pod.
    pub fn identity(&self) -> Result<String, String> {
        self.identity
            .clone()
            .or_else(|| std::env::var("POD_NAME").ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "leader_election : identité absente (POD_NAME)".to_string())
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8443".parse().expect("adresse valide"),
            ui_listen: "127.0.0.1:8081".parse().expect("adresse valide"),
            tls: None,
            auth: AuthConfig::Kubernetes,
            store: "file:///var/lib/mimic-controller".into(),
            // Historique court : les exemples par endpoint couvrent le reste (spec §5.5.3).
            retention: Duration::from_secs(24 * 3600),
            max_exchanges_per_workload: 200_000,
            max_exchanges_total: 1_000_000,
            samples_per_operation: 20,
            max_bytes_per_workload: 256 * 1024 * 1024,
            max_exemplars_per_workload: mimic_core::exemplar::DEFAULT_MAX_ENDPOINTS,
            max_exemplar_bytes_per_workload: 64 * 1024 * 1024,
            max_exemplar_bytes_total: 512 * 1024 * 1024,
            discovery_interval: Duration::from_secs(30),
            snapshot: SnapshotConfig::default(),
            pod_timeout: Duration::from_secs(30),
            pod_forget: Duration::from_secs(600),
            leader_election: None,
            aggregated: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotConfig {
    pub max_per_key: usize,
    /// Snapshots gardés par workload au-delà des référencés et des épinglés (CTL-44,
    /// cycle de vie des snapshots) : les plus récents, par date de construction ou
    /// d'import.
    pub keep_last: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_per_key: 50,
            keep_last: 5,
        }
    }
}

/// Authentification des sidecars (spec §7.1).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum AuthConfig {
    /// Jetons de ServiceAccount validés par `TokenReview` ; active aussi la surveillance
    /// des ressources `MimicWorkload`.
    #[default]
    Kubernetes,
    /// Jetons fixes, pour les tests et le développement hors cluster.
    Static { tokens: BTreeMap<String, Identity> },
}

/// Pod authentifié.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub namespace: String,
    pub pod: String,
    pub workload: String,
    /// Labels du pod, comparés à `spec.selector` (spec §5.5.1).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ControllerConfig {
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, String> {
        let config: Self = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("lecture de {}: {e}", p.display()))?;
                serde_yaml::from_str(&text)
                    .map_err(|e| format!("configuration {} invalide: {e}", p.display()))?
            }
            None => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.aggregated.is_some() && self.tls.is_none() {
            return Err("aggregated exige tls (l'API server joint le contrôleur en HTTPS)".into());
        }
        if !self.store.starts_with("file://") && !self.store.starts_with("s3://") {
            return Err(format!("store non supporté: {}", self.store));
        }
        if self.max_exchanges_per_workload == 0 || self.max_exchanges_total == 0 {
            return Err(
                "max_exchanges_per_workload et max_exchanges_total doivent être positifs".into(),
            );
        }
        if self.max_exemplars_per_workload == 0 {
            return Err("max_exemplars_per_workload doit être positif".into());
        }
        if self.max_exemplar_bytes_per_workload == 0 || self.max_exemplar_bytes_total == 0 {
            return Err(
                "max_exemplar_bytes_per_workload et max_exemplar_bytes_total doivent être positifs"
                    .into(),
            );
        }
        if self.snapshot.max_per_key == 0 {
            return Err("snapshot.max_per_key doit être positif".into());
        }
        if let Some(le) = &self.leader_election {
            le.timings().validate()?;
            if le.lease.is_empty() || le.namespace.is_empty() {
                return Err("leader_election : lease et namespace requis".into());
            }
        }
        if matches!(self.auth, AuthConfig::Kubernetes) && self.tls.is_none() {
            return Err(
                "auth kubernetes exige tls : les sidecars n'acceptent le contrôleur qu'en HTTPS"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Durées exprimées en secondes dans le YAML.
mod duration_secs {
    use super::*;
    use serde::Deserializer;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = f64::deserialize(d)?;
        Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubernetes_auth_requires_tls() {
        assert!(ControllerConfig::default().validate().is_err());
        let c = ControllerConfig {
            tls: Some(TlsFiles::new("c", "k")),
            ..ControllerConfig::default()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn yaml_example() {
        let c: ControllerConfig = serde_yaml::from_str(
            "listen: 0.0.0.0:8443\nauth: { kind: static, tokens: { t: { namespace: a, pod: p, workload: w } } }\nretention: 60\nsnapshot: { max_per_key: 5 }\n",
        )
        .unwrap();
        assert_eq!(c.retention, Duration::from_secs(60));
        assert_eq!(c.snapshot.max_per_key, 5);
        assert!(matches!(c.auth, AuthConfig::Static { .. }));
    }
}
