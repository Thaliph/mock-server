//! Configuration du webhook (`mimic-injector --config`, spec §9.4).

use std::net::{IpAddr, SocketAddr};

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
    /// Image du sidecar (`mimic-proxy`), sans shell ni iptables (spec §9.4b).
    pub image: String,
    /// Image de `mimic-init` (iptables) ; requise sauf en variante CNI.
    #[serde(default)]
    pub init_image: Option<String>,
    #[serde(default)]
    pub image_pull_policy: Option<String>,
    /// CIDR jamais redirigés vers le sidecar, en plus de l'API Kubernetes.
    #[serde(default)]
    pub exclude_outbound_cidrs: Vec<String>,
    /// Règles `ip6tables` aussi (`MIMIC_IPV6`), pour un cluster double pile.
    #[serde(default)]
    pub ipv6: bool,
    /// Ressources de `mimic-init` ; absentes, celles par défaut (spec §9.4).
    #[serde(default)]
    pub init_resources: Option<ResourceRequirements>,
    /// IP du Service `kubernetes` (`KUBERNETES_SERVICE_HOST` au démarrage) : toujours exclue.
    #[serde(skip)]
    pub kubernetes_service_host: Option<IpAddr>,
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
    /// Filtre de logs des sidecars (`MIMIC_LOG`) ; l'annotation `mimic.platform.io/log`
    /// l'emporte. Absent : `info`.
    #[serde(default)]
    pub sidecar_log: Option<String>,
    /// Variante CNI (spec §3.1b) : règles posées par le plugin `mimic-cni`, plus de
    /// `mimic-init` privilégié dans les pods.
    #[serde(default)]
    pub cni: bool,
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
        Self::parse(&text, |k| std::env::var(k).ok())
            .map_err(|e| format!("configuration {} invalide: {e}", path.display()))
    }

    /// Configuration lue d'un texte YAML, complétée par l'environnement du webhook
    /// (`KUBERNETES_SERVICE_HOST`, posé par le kubelet), puis validée.
    pub fn parse(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut config: Self = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        if let Some(host) = env("KUBERNETES_SERVICE_HOST").filter(|h| !h.trim().is_empty()) {
            match host.trim().parse::<IpAddr>() {
                Ok(ip) => config.kubernetes_service_host = Some(ip),
                Err(_) => tracing::warn!(
                    %host,
                    "KUBERNETES_SERVICE_HOST n'est pas une adresse IP : API Kubernetes non exclue d'office"
                ),
            }
        }
        config.validate()?;
        Ok(config)
    }

    /// Refuse au démarrage une configuration de sidecar que les sidecars refuseraient.
    pub fn validate(&self) -> Result<(), String> {
        if self.image.trim().is_empty() {
            return Err("image requise".into());
        }
        if !self.cni
            && self
                .init_image
                .as_deref()
                .is_none_or(|i| i.trim().is_empty())
        {
            return Err(
                "init_image requise : image de mimic-init (iptables), sauf avec cni: true".into(),
            );
        }
        serde_yaml::from_str::<mimic_proxy::config::Config>(&self.sidecar_config)
            .map_err(|e| format!("sidecar_config invalide: {e}"))?;
        if let Some(log) = &self.sidecar_log {
            crate::inject::valid_log_filter(log).map_err(|e| format!("sidecar_log: {e}"))?;
        }
        for cidr in &self.exclude_outbound_cidrs {
            mimic_init::parse_cidr(cidr).map_err(|e| format!("exclude_outbound_cidrs: {e}"))?;
        }
        Ok(())
    }
}
