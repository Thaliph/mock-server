//! Plugin CNI chaîné `mimic-cni` (spec §3.1b) : pose, dans le namespace réseau d'un pod
//! injecté, les règles de `mimic-init`, d'après l'annotation [`RULES_ANNOTATION`] du pod.
//! Aucun conteneur privilégié dans le pod : le privilège est celui du runtime qui appelle le
//! plugin sur le nœud.
//!
//! La logique est ici, sans I/O : l'accès à l'API et l'entrée dans le namespace réseau
//! passent par [`Host`].

use std::collections::BTreeMap;

use mimic_init::apply::Settings;
use serde_json::{Value, json};

pub mod install;

/// Annotation posée par le webhook en variante CNI : variables de `mimic-init`, en JSON.
pub const RULES_ANNOTATION: &str = "mimic.platform.io/cni-rules";
/// Versions du protocole CNI acceptées.
pub const SUPPORTED: [&str; 5] = ["0.3.0", "0.3.1", "0.4.0", "1.0.0", "1.1.0"];
/// Kubeconfig par défaut, écrit par l'installation.
pub const DEFAULT_KUBECONFIG: &str = "/etc/cni/net.d/mimic-cni.d/kubeconfig";
/// Code d'erreur CNI propre au plugin (les codes sous 100 sont réservés).
const PLUGIN_ERROR: u32 = 100;

/// Ce que l'hôte fournit au plugin.
pub trait Host {
    /// Annotations du pod, lues dans l'API avec ce kubeconfig.
    fn pod_annotations(
        &self,
        kubeconfig: &str,
        namespace: &str,
        name: &str,
    ) -> Result<BTreeMap<String, String>, String>;
    /// Pose les règles dans le namespace réseau `netns`.
    fn apply(&self, netns: &str, settings: &Settings) -> Result<(), String>;
}

/// Appel du runtime : variables `CNI_*` et configuration réseau sur l'entrée standard.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// `CNI_COMMAND`.
    pub command: String,
    /// `CNI_NETNS`.
    pub netns: Option<String>,
    /// `CNI_ARGS`, `clé=valeur` séparés par `;`.
    pub args: String,
    pub stdin: Vec<u8>,
}

/// Réponse au runtime : sortie standard et code de sortie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub code: i32,
}

/// Traite un appel du runtime.
pub fn run(inv: &Invocation, host: &dyn Host) -> Outcome {
    let conf: Value = serde_json::from_slice(&inv.stdin).unwrap_or(Value::Null);
    let version = conf["cniVersion"].as_str().unwrap_or("1.0.0").to_string();
    match inv.command.as_str() {
        "VERSION" => ok(json!({"cniVersion": version, "supportedVersions": SUPPORTED})),
        // Le namespace réseau disparaît avec le pod : rien à défaire ni à vérifier.
        "DEL" | "CHECK" => Outcome {
            stdout: String::new(),
            code: 0,
        },
        "ADD" => match add(inv, &conf, host) {
            Ok(()) => ok(previous(&conf, &version)),
            Err(e) => error(&version, &e),
        },
        other => error(&version, &format!("CNI_COMMAND inconnue : {other}")),
    }
}

fn ok(v: Value) -> Outcome {
    Outcome {
        stdout: v.to_string(),
        code: 0,
    }
}

/// Erreur au format CNI : le runtime fait échouer le bac à sable, et le kubelet réessaie.
fn error(version: &str, msg: &str) -> Outcome {
    Outcome {
        stdout: json!({
            "cniVersion": version,
            "code": PLUGIN_ERROR,
            "msg": "mimic-cni",
            "details": msg,
        })
        .to_string(),
        code: 1,
    }
}

/// Résultat du plugin précédent, rendu tel quel (plugin chaîné).
fn previous(conf: &Value, version: &str) -> Value {
    match &conf["prevResult"] {
        Value::Object(_) => conf["prevResult"].clone(),
        _ => json!({"cniVersion": version}),
    }
}

/// `clé=valeur` de `CNI_ARGS`.
fn cni_args(args: &str) -> BTreeMap<&str, &str> {
    args.split(';')
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

fn add(inv: &Invocation, conf: &Value, host: &dyn Host) -> Result<(), String> {
    let args = cni_args(&inv.args);
    let (Some(namespace), Some(name)) = (args.get("K8S_POD_NAMESPACE"), args.get("K8S_POD_NAME"))
    else {
        // Hors Kubernetes : rien à faire.
        return Ok(());
    };
    let kubeconfig = conf["kubeconfig"].as_str().unwrap_or(DEFAULT_KUBECONFIG);
    let annotations = host.pod_annotations(kubeconfig, namespace, name)?;
    let Some(rules) = annotations.get(RULES_ANNOTATION) else {
        return Ok(());
    };
    let settings = settings(rules, pod_ip(&conf["prevResult"]))?;
    let netns = inv
        .netns
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or("CNI_NETNS absent : namespace réseau du pod inconnu")?;
    host.apply(netns, &settings)
}

/// Réglages de `mimic-init` tirés de l'annotation ; une variable inconnue est refusée.
pub fn settings(annotation: &str, pod_ip: Option<String>) -> Result<Settings, String> {
    let map: BTreeMap<String, String> = serde_json::from_str(annotation)
        .map_err(|e| format!("annotation {RULES_ANNOTATION} illisible : {e}"))?;
    let mut s = Settings {
        pod_ip,
        ..Settings::default()
    };
    for (k, v) in map {
        match k.as_str() {
            "MIMIC_INBOUND_PORTS" => s.inbound_ports = v,
            "MIMIC_EXCLUDE_OUTBOUND_CIDRS" => s.exclude_outbound_cidrs = v,
            "MIMIC_EXCLUDE_OUTBOUND_PORTS" => s.exclude_outbound_ports = v,
            "MIMIC_MESH" => s.mesh = v,
            "MIMIC_IPV6" => s.ipv6 = v == "true",
            // Lancé en root sur le nœud : jamais un programme choisi par le pod.
            "MIMIC_IPTABLES" => {
                s.iptables = mimic_init::parse_iptables(&v)
                    .map_err(|e| format!("annotation {RULES_ANNOTATION} : MIMIC_IPTABLES {e}"))?;
            }
            other => {
                return Err(format!(
                    "annotation {RULES_ANNOTATION} : variable inconnue {other}"
                ));
            }
        }
    }
    Ok(s)
}

/// Première IP du résultat précédent, sans son préfixe.
fn pod_ip(prev: &Value) -> Option<String> {
    let address = prev["ips"][0]["address"].as_str()?;
    Some(address.split('/').next().unwrap_or(address).to_string())
}
