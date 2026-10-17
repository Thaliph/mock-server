//! Configuration du sidecar (spec §9.1) : fichier YAML, surchargé par l'environnement.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use mimic_core::mock::Strategy;
use serde::{Deserialize, Serialize};

/// Mode de fonctionnement (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Passthrough,
    #[default]
    Record,
    Mock,
    MockPartial,
}

impl Mode {
    pub const ALL: [Mode; 4] = [
        Mode::Passthrough,
        Mode::Record,
        Mode::Mock,
        Mode::MockPartial,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Passthrough => "passthrough",
            Mode::Record => "record",
            Mode::Mock => "mock",
            Mode::MockPartial => "mock-partial",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.as_str() == s.trim())
    }

    pub(crate) fn to_u8(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_u8(v: u8) -> Mode {
        Mode::ALL[v as usize % Mode::ALL.len()]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    /// Identifiant de l'instance, en pratique le nom du pod.
    pub instance: String,
    pub workload: WorkloadConfig,
    pub inbound: InboundConfig,
    pub outbound: OutboundConfig,
    pub admin: AdminConfig,
    pub capture: CaptureConfig,
    pub redact: RedactSection,
    pub mock: MockSection,
    pub store: StoreConfig,
    pub share: Option<ShareConfig>,
    /// Fichier d'annotations du pod exposé par l'API downward, surveillé pour la bascule de mode.
    pub annotations_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Record,
            instance: "local".into(),
            workload: WorkloadConfig::default(),
            inbound: InboundConfig::default(),
            outbound: OutboundConfig::default(),
            admin: AdminConfig::default(),
            capture: CaptureConfig::default(),
            redact: RedactSection::default(),
            mock: MockSection::default(),
            store: StoreConfig::default(),
            share: None,
            annotations_file: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkloadConfig {
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboundConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    /// Adresse de l'application. Absente : port de destination d'origine sur 127.0.0.1
    /// (redirection iptables).
    pub upstream: Option<SocketAddr>,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: ([0, 0, 0, 0], 15006).into(),
            upstream: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboundConfig {
    pub enabled: bool,
    /// Accepte les requêtes de proxy explicite (`HTTP_PROXY`) et le trafic redirigé par iptables.
    pub listen: SocketAddr,
}

impl Default for OutboundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: ([127, 0, 0, 1], 15001).into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    pub listen: SocketAddr,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            listen: ([0, 0, 0, 0], 15000).into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub bodies: bool,
    pub max_body_bytes: usize,
    /// Bodies conservés par (sens, authority, méthode, chemin, code) ; au-delà, métadonnées seules.
    pub samples_per_operation: usize,
    /// Nombre maximal d'échanges en mémoire ; les plus anciens sont évincés.
    pub max_exchanges: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            bodies: true,
            max_body_bytes: 1024 * 1024,
            samples_per_operation: 20,
            max_exchanges: 50_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedactSection {
    pub extra_json_paths: Vec<String>,
    /// Sel des marqueurs de redaction. À partager entre les réplicas d'un workload (Secret)
    /// pour que les marqueurs restent cohérents ; absent : sel aléatoire par processus.
    pub salt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unmatched {
    #[default]
    Error,
    Passthrough,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MockSection {
    pub strategy: StrategyName,
    pub unmatched: Unmatched,
    pub latency_factor: f64,
    pub match_headers: Vec<String>,
    pub seed: u64,
    /// Ajoute `x-mimic-match` aux réponses mockées.
    pub debug_header: bool,
}

impl Default for MockSection {
    fn default() -> Self {
        Self {
            strategy: StrategyName::Best,
            unmatched: Unmatched::Error,
            latency_factor: 1.0,
            match_headers: vec![],
            seed: 0x5EED,
            debug_header: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrategyName {
    #[default]
    Best,
    RoundRobin,
    Sequence,
    WeightedStatus,
}

impl From<StrategyName> for Strategy {
    fn from(s: StrategyName) -> Self {
        match s {
            StrategyName::Best => Strategy::Best,
            StrategyName::RoundRobin => Strategy::RoundRobin,
            StrategyName::Sequence => Strategy::Sequence,
            StrategyName::WeightedStatus => Strategy::WeightedStatus,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Répertoire de persistance locale (JSONL). Absent : mémoire seule.
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareConfig {
    /// `file:///chemin` ou `s3://bucket/prefixe`.
    pub url: String,
    #[serde(default = "default_share_interval", with = "duration_secs")]
    pub interval: Duration,
    #[serde(default = "default_share_retention", with = "duration_secs")]
    pub retention: Duration,
}

fn default_share_interval() -> Duration {
    Duration::from_secs(10)
}

fn default_share_retention() -> Duration {
    Duration::from_secs(7 * 24 * 3600)
}

/// Durées exprimées en secondes dans le YAML.
mod duration_secs {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = f64::deserialize(d)?;
        Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom)
    }
}

impl Config {
    /// Lit un fichier YAML puis applique les variables d'environnement
    /// `MIMIC_MODE`, `POD_NAME`, `POD_NAMESPACE`, `MIMIC_WORKLOAD`, `MIMIC_REDACT_SALT`.
    pub fn load(path: Option<&std::path::Path>) -> Result<Config, String> {
        let mut config = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("lecture de {}: {e}", p.display()))?;
                serde_yaml::from_str(&text)
                    .map_err(|e| format!("configuration {} invalide: {e}", p.display()))?
            }
            None => Config::default(),
        };
        config.apply_env(|k| std::env::var(k).ok())?;
        config.validate()?;
        Ok(config)
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        if let Some(m) = get("MIMIC_MODE") {
            self.mode = Mode::parse(&m).ok_or_else(|| format!("MIMIC_MODE inconnu: {m}"))?;
        }
        if let Some(v) = get("POD_NAME") {
            self.instance = v;
        }
        if let Some(v) = get("POD_NAMESPACE") {
            self.workload.namespace = v;
        }
        if let Some(v) = get("MIMIC_WORKLOAD") {
            self.workload.name = v;
        }
        if let Some(v) = get("MIMIC_REDACT_SALT") {
            self.redact.salt = Some(v);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=100.0).contains(&self.mock.latency_factor) {
            return Err("mock.latency_factor doit être entre 0 et 100".into());
        }
        if self.capture.max_exchanges == 0 {
            return Err("capture.max_exchanges doit être positif".into());
        }
        if let Some(share) = &self.share {
            if self.workload.name.is_empty() {
                return Err("share exige un nom de workload (MIMIC_WORKLOAD)".into());
            }
            if !share.url.starts_with("file://") && !share.url.starts_with("s3://") {
                return Err(format!("share.url non supportée: {}", share.url));
            }
        }
        if self
            .instance
            .contains(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_'))
        {
            return Err(format!("instance invalide: {}", self.instance));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_with_defaults() {
        let c: Config = serde_yaml::from_str(
            "mode: mock\nmock:\n  strategy: weighted-status\n  latency_factor: 2\n",
        )
        .unwrap();
        assert_eq!(c.mode, Mode::Mock);
        assert_eq!(c.mock.strategy, StrategyName::WeightedStatus);
        assert_eq!(c.capture.samples_per_operation, 20);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(serde_yaml::from_str::<Config>("mood: mock\n").is_err());
    }

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        c.apply_env(|k| match k {
            "MIMIC_MODE" => Some("mock-partial".into()),
            "POD_NAME" => Some("api-7d9f-x".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.mode, Mode::MockPartial);
        assert_eq!(c.instance, "api-7d9f-x");
        assert!(c.apply_env(|_| Some("nope".into())).is_err());
    }

    #[test]
    fn share_requires_workload() {
        let c = Config {
            share: Some(ShareConfig {
                url: "file:///tmp/x".into(),
                interval: default_share_interval(),
                retention: default_share_retention(),
            }),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }
}
