//! Webhook and injection settings of the controller (spec §10.2).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use k8s_openapi::api::core::v1::ResourceRequirements;
use mimic_core::inject::InjectConfig;
use mimic_runtime::config::{ConfigError, Env};

const DEFAULT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8443);
const DEFAULT_RESOURCES: &str =
    r#"{"requests":{"cpu":"50m","memory":"64Mi"},"limits":{"memory":"128Mi"}}"#;

pub struct WebhookConfig {
    pub addr: SocketAddr,
    /// Namespace of the controller, never injected.
    pub namespace: String,
    pub service: String,
    pub secret: String,
    pub configuration: String,
    pub inject: InjectConfig,
}

impl WebhookConfig {
    pub fn from_env(env: &Env, version: &str) -> Result<WebhookConfig, ConfigError> {
        let resources = env
            .optional("MIMIC_INJECT_SIDECAR_RESOURCES")
            .unwrap_or(DEFAULT_RESOURCES);
        let sidecar_resources: ResourceRequirements = serde_json::from_str(resources)
            .map_err(|e| ConfigError::invalid("MIMIC_INJECT_SIDECAR_RESOURCES", resources, e))?;
        let sidecar_log = env.optional("MIMIC_INJECT_SIDECAR_LOG").unwrap_or("info");
        mimic_runtime::logging::filter(sidecar_log)
            .map_err(|e| ConfigError::invalid("MIMIC_INJECT_SIDECAR_LOG", sidecar_log, e))?;
        Ok(WebhookConfig {
            addr: env.parse_or("MIMIC_WEBHOOK_ADDR", DEFAULT_ADDR)?,
            namespace: env.required("MIMIC_NAMESPACE")?.to_string(),
            service: env.required("MIMIC_WEBHOOK_SERVICE")?.to_string(),
            secret: env.required("MIMIC_WEBHOOK_SECRET")?.to_string(),
            configuration: env.required("MIMIC_WEBHOOK_CONFIGURATION")?.to_string(),
            inject: InjectConfig {
                version: version.to_string(),
                sidecar_image: env.required("MIMIC_INJECT_SIDECAR_IMAGE")?.to_string(),
                init_image: env.required("MIMIC_INJECT_INIT_IMAGE")?.to_string(),
                sidecar_resources,
                sidecar_log: sidecar_log.to_string(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED: [(&str, &str); 6] = [
        ("MIMIC_NAMESPACE", "mimic-system"),
        ("MIMIC_WEBHOOK_SERVICE", "mimic"),
        ("MIMIC_WEBHOOK_SECRET", "mimic-webhook-tls"),
        ("MIMIC_WEBHOOK_CONFIGURATION", "mimic-injector"),
        ("MIMIC_INJECT_SIDECAR_IMAGE", "sidecar:1"),
        ("MIMIC_INJECT_INIT_IMAGE", "init:1"),
    ];

    fn env(extra: &[(&str, &str)]) -> Env {
        Env::from_pairs(
            REQUIRED
                .iter()
                .chain(extra)
                .map(|(k, v)| (k.to_string(), v.to_string())),
        )
    }

    #[test]
    fn inj_08_defaults() {
        let cfg = WebhookConfig::from_env(&env(&[]), "1.2.3").unwrap();
        assert_eq!(cfg.addr, "0.0.0.0:8443".parse().unwrap());
        assert_eq!(cfg.inject.sidecar_log, "info");
        assert_eq!(cfg.inject.version, "1.2.3");
        let limits = cfg.inject.sidecar_resources.limits.unwrap();
        assert_eq!(limits["memory"].0, "128Mi");
    }

    #[test]
    fn inj_08_each_required_variable_is_required() {
        for (var, _) in REQUIRED {
            let pairs = REQUIRED.iter().filter(|(k, _)| *k != var);
            let env = Env::from_pairs(pairs.map(|(k, v)| (k.to_string(), v.to_string())));
            let err = WebhookConfig::from_env(&env, "1").err().expect(var);
            assert!(err.to_string().contains(var), "{var}: {err}");
        }
    }

    #[test]
    fn inj_08_invalid_values_are_rejected() {
        for (var, value) in [
            ("MIMIC_WEBHOOK_ADDR", "nope"),
            ("MIMIC_INJECT_SIDECAR_RESOURCES", "{not json"),
            ("MIMIC_INJECT_SIDECAR_LOG", "info,=="),
        ] {
            let err = WebhookConfig::from_env(&env(&[(var, value)]), "1")
                .err()
                .expect(var);
            assert!(err.to_string().contains(var), "{var}: {err}");
        }
    }
}
