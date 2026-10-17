//! Mutation d'un pod : fonction pure du pod et de la configuration (spec §9.4).

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, ObjectFieldSelector, Pod, Probe, ProjectedVolumeSource,
    SecretKeySelector, SecurityContext, ServiceAccountTokenProjection, Volume, VolumeMount,
    VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::config::InjectorConfig;

pub const ANNOTATION_INJECT: &str = "mimic.platform.io/inject";
pub const ANNOTATION_STATUS: &str = "mimic.platform.io/status";
pub const ANNOTATION_WORKLOAD: &str = "mimic.platform.io/workload";
pub const ANNOTATION_CONFIG: &str = "mimic.platform.io/config";
pub const ANNOTATION_INBOUND_PORTS: &str = "mimic.platform.io/inbound-ports";
pub const ANNOTATION_EXCLUDE_CIDRS: &str = "mimic.platform.io/exclude-outbound-cidrs";
pub const ANNOTATION_EXCLUDE_PORTS: &str = "mimic.platform.io/exclude-outbound-ports";
pub const ANNOTATION_TLS_INTERCEPT: &str = "mimic.platform.io/tls-intercept";
pub const LABEL_SIDECAR: &str = "mimic.platform.io/sidecar";

pub const SIDECAR_UID: i64 = 1337;
pub const ADMIN_PORT: i32 = 15000;
pub const CONFIG_DIR: &str = "/etc/mimic";
pub const STORE_DIR: &str = "/var/lib/mimic";
pub const TOKEN_DIR: &str = "/var/run/secrets/mimic";
pub const CONTROLLER_CA_DIR: &str = "/etc/mimic-ca";
pub const CA_DIR: &str = "/var/run/mimic/ca";

/// Conteneurs de service mesh avec lesquels Mimic ne cohabite pas (spec §3.3).
const MESH_CONTAINERS: &[&str] = &["istio-proxy", "linkerd-proxy"];

#[derive(Debug)]
pub enum Outcome {
    Injected(Box<Pod>),
    /// Pod admis tel quel ; la raison devient un avertissement d'admission.
    Skipped(String),
}

fn annotation<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn label<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata
        .labels
        .as_ref()?
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

/// Nom du workload : annotation, sinon `app.kubernetes.io/name`, sinon `app` (spec §5.4).
pub fn workload_name(pod: &Pod) -> Option<&str> {
    annotation(pod, ANNOTATION_WORKLOAD)
        .or_else(|| label(pod, "app.kubernetes.io/name"))
        .or_else(|| label(pod, "app"))
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn skip_reason(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref()?;
    if annotation(pod, ANNOTATION_STATUS) == Some("injected")
        || spec
            .init_containers
            .iter()
            .flatten()
            .any(|c| c.name == "mimic")
    {
        return Some("mimic déjà injecté".into());
    }
    if annotation(pod, ANNOTATION_INJECT) == Some("false") {
        return Some(format!("injection désactivée par {ANNOTATION_INJECT}"));
    }
    if spec.host_network == Some(true) {
        return Some("hostNetwork : les règles iptables toucheraient le noeud".into());
    }
    let all = spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten());
    if let Some(c) = all
        .into_iter()
        .find(|c| MESH_CONTAINERS.contains(&c.name.as_str()))
    {
        return Some(format!("service mesh détecté ({}), hors périmètre", c.name));
    }
    None
}

/// Ports entrants à intercepter : annotation, sinon ports TCP déclarés, sinon tous.
fn inbound_ports(pod: &Pod) -> String {
    if let Some(a) = annotation(pod, ANNOTATION_INBOUND_PORTS) {
        return a.replace(' ', "");
    }
    let ports: BTreeSet<i32> = pod
        .spec
        .iter()
        .flat_map(|s| s.containers.iter())
        .flat_map(|c| c.ports.iter().flatten())
        .filter(|p| p.protocol.as_deref().unwrap_or("TCP") == "TCP")
        .map(|p| p.container_port)
        .collect();
    if ports.is_empty() {
        return "*".into();
    }
    ports
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn env(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: Some(value.into()),
        ..EnvVar::default()
    }
}

fn field_env(name: &str, path: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                field_path: path.into(),
                ..ObjectFieldSelector::default()
            }),
            ..EnvVarSource::default()
        }),
        ..EnvVar::default()
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.into(),
        mount_path: path.into(),
        read_only: read_only.then_some(true),
        ..VolumeMount::default()
    }
}

fn probe(path: &str, period: i32, failures: i32) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.into()),
            port: IntOrString::String("mimic-admin".into()),
            ..HTTPGetAction::default()
        }),
        period_seconds: Some(period),
        failure_threshold: Some(failures),
        ..Probe::default()
    }
}

fn init_container(pod: &Pod, cfg: &InjectorConfig) -> Container {
    let mut cidrs = cfg.exclude_outbound_cidrs.clone();
    if let Some(a) = annotation(pod, ANNOTATION_EXCLUDE_CIDRS) {
        cidrs.extend(split_list(a));
    }
    let mut envs = vec![
        env("MIMIC_INBOUND_PORTS", inbound_ports(pod)),
        env("MIMIC_EXCLUDE_OUTBOUND_CIDRS", cidrs.join(",")),
    ];
    if let Some(a) = annotation(pod, ANNOTATION_EXCLUDE_PORTS) {
        envs.push(env("MIMIC_EXCLUDE_OUTBOUND_PORTS", split_list(a).join(",")));
    }
    if let Some(bin) = &cfg.iptables {
        envs.push(env("MIMIC_IPTABLES", bin.clone()));
    }
    Container {
        name: "mimic-init".into(),
        image: Some(cfg.image.clone()),
        image_pull_policy: cfg.image_pull_policy.clone(),
        command: Some(vec!["/usr/local/bin/mimic-init".into()]),
        env: Some(envs),
        security_context: Some(SecurityContext {
            run_as_user: Some(0),
            run_as_non_root: Some(false),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".into()]),
                add: Some(vec!["NET_ADMIN".into(), "NET_RAW".into()]),
            }),
            ..SecurityContext::default()
        }),
        ..Container::default()
    }
}

fn sidecar(pod: &Pod, cfg: &InjectorConfig, workload: &str, intercept: &[String]) -> Container {
    let mut envs = vec![
        field_env("POD_NAME", "metadata.name"),
        field_env("POD_NAMESPACE", "metadata.namespace"),
        env("MIMIC_WORKLOAD", workload),
        EnvVar {
            name: "MIMIC_REDACT_SALT".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: format!("{workload}-mimic"),
                    key: "redact-salt".into(),
                    optional: Some(true),
                }),
                ..EnvVarSource::default()
            }),
            ..EnvVar::default()
        },
    ];
    let mut mounts = vec![
        mount("mimic-store", STORE_DIR, false),
        mount("mimic-token", TOKEN_DIR, true),
        mount("mimic-controller-ca", CONTROLLER_CA_DIR, true),
        mount("mimic-ca", CA_DIR, false),
    ];
    let mut args = None;
    if annotation(pod, ANNOTATION_CONFIG).is_some() {
        mounts.push(mount("mimic-config", CONFIG_DIR, true));
        args = Some(vec!["--config".into(), format!("{CONFIG_DIR}/mimic.yaml")]);
    } else {
        envs.push(env("MIMIC_CONFIG_YAML", cfg.sidecar_config.clone()));
    }
    if !intercept.is_empty() {
        envs.push(env("MIMIC_TLS_INTERCEPT", intercept.join(",")));
    }
    let resources = cfg.resources.clone().unwrap_or_else(|| {
        let q = |s: &str| Quantity(s.into());
        k8s_openapi::api::core::v1::ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), q("50m")),
                ("memory".into(), q("64Mi")),
            ])),
            limits: Some(BTreeMap::from([("memory".into(), q("256Mi"))])),
            ..Default::default()
        }
    });
    Container {
        name: "mimic".into(),
        image: Some(cfg.image.clone()),
        image_pull_policy: cfg.image_pull_policy.clone(),
        // Native sidecar : démarre avant l'application et s'arrête après elle.
        restart_policy: Some("Always".into()),
        args,
        env: Some(envs),
        ports: Some(vec![ContainerPort {
            name: Some("mimic-admin".into()),
            container_port: ADMIN_PORT,
            ..ContainerPort::default()
        }]),
        // L'application ne démarre qu'une fois le proxy prêt (et la CA publiée).
        startup_probe: Some(probe("/healthz", 1, 30)),
        readiness_probe: Some(probe("/readyz", 10, 3)),
        liveness_probe: Some(probe("/healthz", 10, 3)),
        resources: Some(resources),
        security_context: Some(SecurityContext {
            run_as_user: Some(SIDECAR_UID),
            run_as_group: Some(SIDECAR_UID),
            run_as_non_root: Some(true),
            read_only_root_filesystem: Some(true),
            allow_privilege_escalation: Some(false),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".into()]),
                add: None,
            }),
            ..SecurityContext::default()
        }),
        volume_mounts: Some(mounts),
        ..Container::default()
    }
}

fn volumes(pod: &Pod, cfg: &InjectorConfig) -> Vec<Volume> {
    let mut v = vec![
        Volume {
            name: "mimic-store".into(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(Quantity("512Mi".into())),
                ..EmptyDirVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: "mimic-token".into(),
            projected: Some(ProjectedVolumeSource {
                sources: Some(vec![VolumeProjection {
                    service_account_token: Some(ServiceAccountTokenProjection {
                        audience: Some(cfg.token_audience.clone()),
                        expiration_seconds: Some(3600),
                        path: "token".into(),
                    }),
                    ..VolumeProjection::default()
                }]),
                ..ProjectedVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: "mimic-controller-ca".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: cfg.controller_ca_configmap.clone(),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: "mimic-ca".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Volume::default()
        },
    ];
    if let Some(name) = annotation(pod, ANNOTATION_CONFIG) {
        v.push(Volume {
            name: "mimic-config".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: name.into(),
                ..ConfigMapVolumeSource::default()
            }),
            ..Volume::default()
        });
    }
    v
}

/// Fait confiance à la CA Mimic dans un conteneur de l'app (spec §4.2), sans écraser
/// une variable que l'app définit déjà.
fn trust_ca(c: &mut Container) {
    let bundle = format!("{CA_DIR}/ca-bundle.crt");
    let ca = format!("{CA_DIR}/ca.crt");
    let wanted = [
        ("SSL_CERT_FILE", &bundle),
        ("NODE_EXTRA_CA_CERTS", &ca),
        ("REQUESTS_CA_BUNDLE", &bundle),
        ("CURL_CA_BUNDLE", &bundle),
    ];
    let envs = c.env.get_or_insert_with(Vec::new);
    for (name, value) in wanted {
        if !envs.iter().any(|e| e.name == name) {
            envs.push(env(name, value.clone()));
        }
    }
    c.volume_mounts
        .get_or_insert_with(Vec::new)
        .push(mount("mimic-ca", CA_DIR, true));
}

/// Pod tel qu'il doit être admis, ou la raison pour laquelle il reste tel quel.
pub fn mutate(pod: &Pod, cfg: &InjectorConfig) -> Outcome {
    if pod.spec.is_none() {
        return Outcome::Skipped("pod sans spec".into());
    }
    if let Some(reason) = skip_reason(pod) {
        return Outcome::Skipped(reason);
    }
    let Some(workload) = workload_name(pod).map(String::from) else {
        return Outcome::Skipped(format!(
            "aucun nom de workload : poser {ANNOTATION_WORKLOAD} ou le label app.kubernetes.io/name"
        ));
    };
    let intercept = annotation(pod, ANNOTATION_TLS_INTERCEPT)
        .map(split_list)
        .unwrap_or_default();

    let mut out = pod.clone();
    let init = init_container(pod, cfg);
    let side = sidecar(pod, cfg, &workload, &intercept);
    let new_volumes = volumes(pod, cfg);
    let spec = out.spec.get_or_insert_with(Default::default);
    let mut inits = vec![init, side];
    inits.extend(spec.init_containers.take().unwrap_or_default());
    spec.init_containers = Some(inits);
    spec.volumes
        .get_or_insert_with(Vec::new)
        .extend(new_volumes);
    if !intercept.is_empty() {
        spec.containers.iter_mut().for_each(trust_ca);
    }
    out.metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(ANNOTATION_STATUS.into(), "injected".into());
    out.metadata
        .labels
        .get_or_insert_with(BTreeMap::new)
        .insert(LABEL_SIDECAR.into(), "true".into());
    Outcome::Injected(Box::new(out))
}
