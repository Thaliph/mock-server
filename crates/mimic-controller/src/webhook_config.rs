//! Webhook and injection settings of the controller (spec §10.2).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use k8s_openapi::api::core::v1::ResourceRequirements;
use mimic_core::inject::InjectConfig;
use mimic_core::mesh::view::Limits;
use mimic_runtime::config::{ConfigError, Env};

use crate::mesh::MeshConfig;

const DEFAULT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8443);
const DEFAULT_API_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8444);
const DEFAULT_RESOURCES: &str =
    r#"{"requests":{"cpu":"50m","memory":"64Mi"},"limits":{"memory":"128Mi"}}"#;

pub struct WebhookConfig {
    pub addr: SocketAddr,
    pub max_connections: usize,
    pub request_timeout: Duration,
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
        let namespace = env.required("MIMIC_NAMESPACE")?;
        let service = env.required("MIMIC_WEBHOOK_SERVICE")?;
        let api_addr: SocketAddr = env.parse_or("MIMIC_SIDECAR_API_ADDR", DEFAULT_API_ADDR)?;
        let mesh = MeshConfig::from_env(env)?;
        let controller_url = format!("https://{service}.{namespace}.svc:{}", api_addr.port());
        Ok(WebhookConfig {
            addr: env.parse_or("MIMIC_WEBHOOK_ADDR", DEFAULT_ADDR)?,
            max_connections: max_connections(env)?,
            request_timeout: request_timeout(env)?,
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
                controller_url,
                load_enabled: env.parse_or("MIMIC_LOAD_MAX_RUNS", 0_u32)? > 0,
                mesh_env: mesh_env(&mesh),
            },
        })
    }
}

/// The `MIMIC_MESH*` variables the sidecars get (spec §7.1): the switch and the ages always, the
/// bounds of the view only when they are not the defaults.
fn mesh_env(mesh: &MeshConfig) -> Vec<(String, String)> {
    let seconds = |d: Duration| {
        if d.subsec_millis() == 0 {
            format!("{}s", d.as_secs())
        } else {
            format!("{}ms", d.as_millis())
        }
    };
    let mut env = vec![
        (
            "MIMIC_MESH".to_string(),
            if mesh.enabled { "on" } else { "off" }.to_string(),
        ),
        (
            "MIMIC_MESH_STALE_AFTER".to_string(),
            seconds(mesh.stale_after),
        ),
        (
            "MIMIC_MESH_STARTUP_WAIT".to_string(),
            seconds(mesh.startup_wait),
        ),
    ];
    let defaults = Limits::default();
    for (name, value, default) in [
        (
            "MIMIC_MESH_MAX_SERVICES",
            mesh.limits.max_services,
            defaults.max_services,
        ),
        (
            "MIMIC_MESH_MAX_ENDPOINTS",
            mesh.limits.max_endpoints,
            defaults.max_endpoints,
        ),
        (
            "MIMIC_MESH_MAX_VIEW_BYTES",
            mesh.limits.max_view_bytes,
            defaults.max_view_bytes,
        ),
    ] {
        if value != default {
            env.push((name.to_string(), value.to_string()));
        }
    }
    env
}

fn max_connections(env: &Env) -> Result<usize, ConfigError> {
    let var = "MIMIC_WEBHOOK_MAX_CONNECTIONS";
    let value = env.parse_or(var, 256_usize)?;
    if (1..=65535).contains(&value) {
        Ok(value)
    } else {
        Err(ConfigError::invalid(
            var,
            &value.to_string(),
            "expected 1 to 65535",
        ))
    }
}

fn request_timeout(env: &Env) -> Result<Duration, ConfigError> {
    let var = "MIMIC_WEBHOOK_REQUEST_TIMEOUT";
    let value = env.duration_or(var, Duration::from_secs(5))?;
    if value.is_zero() {
        let given = env.optional(var).unwrap_or_default();
        return Err(ConfigError::invalid(
            var,
            given,
            "expected a duration above 0",
        ));
    }
    Ok(value)
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
    fn msh_39_the_mesh_variables_for_the_sidecars() {
        let cfg = WebhookConfig::from_env(&env(&[]), "1").unwrap();
        fn pairs(cfg: &WebhookConfig) -> Vec<(&str, &str)> {
            cfg.inject
                .mesh_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect()
        }
        assert_eq!(
            pairs(&cfg),
            [
                ("MIMIC_MESH", "on"),
                ("MIMIC_MESH_STALE_AFTER", "60s"),
                ("MIMIC_MESH_STARTUP_WAIT", "2s"),
            ]
        );
        let extra = [
            ("MIMIC_MESH", "off"),
            ("MIMIC_MESH_STALE_AFTER", "90s"),
            ("MIMIC_MESH_STARTUP_WAIT", "500ms"),
            ("MIMIC_MESH_MAX_SERVICES", "7"),
            ("MIMIC_MESH_MAX_VIEW_BYTES", "64Ki"),
        ];
        let cfg = WebhookConfig::from_env(&env(&extra), "1").unwrap();
        assert_eq!(
            pairs(&cfg),
            [
                ("MIMIC_MESH", "off"),
                ("MIMIC_MESH_STALE_AFTER", "90s"),
                ("MIMIC_MESH_STARTUP_WAIT", "500ms"),
                ("MIMIC_MESH_MAX_SERVICES", "7"),
                ("MIMIC_MESH_MAX_VIEW_BYTES", "65536"),
            ]
        );
        // An invalid value exits 2 (the controller's own parsing).
        for (var, value) in [
            ("MIMIC_MESH_STALE_AFTER", "0s"),
            ("MIMIC_MESH_STALE_AFTER", "2h"),
            ("MIMIC_MESH_STARTUP_WAIT", "61s"),
        ] {
            let err = WebhookConfig::from_env(&env(&[(var, value)]), "1").err();
            assert!(err.is_some_and(|e| e.to_string().contains(var)), "{var}");
        }
    }

    #[test]
    fn tls_12_controller_url_names_the_service_and_the_sidecar_api_port() {
        let cfg = WebhookConfig::from_env(&env(&[]), "1").unwrap();
        assert_eq!(
            cfg.inject.controller_url,
            "https://mimic.mimic-system.svc:8444"
        );
        let extra = [("MIMIC_SIDECAR_API_ADDR", "0.0.0.0:9444")];
        let cfg = WebhookConfig::from_env(&env(&extra), "1").unwrap();
        assert_eq!(
            cfg.inject.controller_url,
            "https://mimic.mimic-system.svc:9444"
        );
    }

    #[test]
    fn inj_19_listener_limit_defaults_and_values() {
        let cfg = WebhookConfig::from_env(&env(&[]), "1").unwrap();
        assert_eq!(cfg.max_connections, 256);
        assert_eq!(cfg.request_timeout, Duration::from_secs(5));
        let extra = [
            ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "4"),
            ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "500ms"),
        ];
        let cfg = WebhookConfig::from_env(&env(&extra), "1").unwrap();
        assert_eq!(cfg.max_connections, 4);
        assert_eq!(cfg.request_timeout, Duration::from_millis(500));
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
            ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "0"),
            ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "65536"),
            ("MIMIC_WEBHOOK_MAX_CONNECTIONS", "-1"),
            ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "0s"),
            ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "0ms"),
            ("MIMIC_WEBHOOK_REQUEST_TIMEOUT", "5"),
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
