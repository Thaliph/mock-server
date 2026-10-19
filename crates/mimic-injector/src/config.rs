//! Configuration du webhook (`mimic-injector --config`, spec §9.4).

use std::net::SocketAddr;

use k8s_openapi::api::core::v1::ResourceRequirements;
pub use mimic_net::tls::TlsFiles;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectorConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Certificat servi à l'API server ; absent, HTTP en clair (tests uniquement).
    #[serde(default)]
    pub tls: Option<TlsFiles>,
    /// Image unique qui porte `mimic-proxy` et `mimic-init`.
    pub image: String,
    #[serde(default)]
    pub image_pull_policy: Option<String>,
    /// CIDR jamais redirigés vers le sidecar (API Kubernetes en tête).
    #[serde(default)]
    pub exclude_outbound_cidrs: Vec<String>,
    /// Binaire iptables de `mimic-init` (`iptables-legacy` selon les noeuds).
    #[serde(default)]
    pub iptables: Option<String>,
    /// Configuration par défaut des sidecars (YAML de la spec §9.1), passée en ligne.
    pub sidecar_config: String,
    #[serde(default)]
    pub resources: Option<ResourceRequirements>,
    /// ConfigMap de la CA du contrôleur, publié dans les namespaces par trust-manager.
    #[serde(default = "default_controller_ca")]
    pub controller_ca_configmap: String,
    /// Audience du jeton projeté, vérifiée par le contrôleur (spec §7.1).
    #[serde(default = "default_audience")]
    pub token_audience: String,
    /// Label du pod repris dans `MIMIC_VERSION`, label `version` des métriques (spec §9.3).
    #[serde(default = "default_version_label")]
    pub version_label: String,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8443))
}

fn default_controller_ca() -> String {
    "mimic-controller-ca".into()
}

fn default_version_label() -> String {
    "app.kubernetes.io/version".into()
}

fn default_audience() -> String {
    "mimic-controller".into()
}

impl InjectorConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("lecture de {}: {e}", path.display()))?;
        let config: Self = serde_yaml::from_str(&text)
            .map_err(|e| format!("configuration {} invalide: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Refuse au démarrage une configuration de sidecar que les sidecars refuseraient.
    pub fn validate(&self) -> Result<(), String> {
        if self.image.trim().is_empty() {
            return Err("image requise".into());
        }
        serde_yaml::from_str::<mimic_proxy::config::Config>(&self.sidecar_config)
            .map_err(|e| format!("sidecar_config invalide: {e}"))?;
        Ok(())
    }
}
