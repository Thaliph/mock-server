//! Critère CNI-01 de la spec (§3.1b) : plugin `mimic-cni`, avec un hôte simulé.

use std::collections::BTreeMap;
use std::sync::Mutex;

use mimic_cni::{Host, Invocation, RULES_ANNOTATION, run};
use mimic_init::apply::Settings;
use serde_json::{Value, json};

/// Hôte simulé : annotations du pod et règles posées.
#[derive(Default)]
struct Fake {
    annotations: Option<BTreeMap<String, String>>,
    applied: Mutex<Vec<(String, Settings)>>,
    fail_apply: bool,
}

impl Host for Fake {
    fn pod_annotations(
        &self,
        kubeconfig: &str,
        namespace: &str,
        name: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        assert_eq!(kubeconfig, "/etc/cni/net.d/mimic-cni.d/kubeconfig");
        assert_eq!((namespace, name), ("shop", "checkout-7d9f-a"));
        self.annotations
            .clone()
            .ok_or_else(|| "pods \"checkout-7d9f-a\" not found".to_string())
    }

    fn apply(&self, netns: &str, settings: &Settings) -> Result<(), String> {
        if self.fail_apply {
            return Err("iptables-restore a échoué (exit status: 1)".into());
        }
        self.applied
            .lock()
            .unwrap()
            .push((netns.to_string(), settings.clone()));
        Ok(())
    }
}

fn prev() -> Value {
    json!({
        "cniVersion": "1.0.0",
        "interfaces": [{"name": "eth0", "sandbox": "/var/run/netns/cni-1"}],
        "ips": [{"address": "10.244.1.7/24", "gateway": "10.244.1.1", "interface": 0}]
    })
}

fn invocation(command: &str) -> Invocation {
    Invocation {
        command: command.into(),
        netns: Some("/var/run/netns/cni-1".into()),
        args: "IgnoreUnknown=1;K8S_POD_NAMESPACE=shop;K8S_POD_NAME=checkout-7d9f-a;K8S_POD_INFRA_CONTAINER_ID=abc".into(),
        stdin: serde_json::to_vec(&json!({
            "cniVersion": "1.0.0",
            "name": "k8s-pod-network",
            "type": "mimic-cni",
            "kubeconfig": "/etc/cni/net.d/mimic-cni.d/kubeconfig",
            "prevResult": prev()
        }))
        .unwrap(),
    }
}

fn rules(map: Value) -> BTreeMap<String, String> {
    BTreeMap::from([(RULES_ANNOTATION.to_string(), map.to_string())])
}

#[test]
fn cni_01_version_lists_supported_versions() {
    let out = run(&invocation("VERSION"), &Fake::default());
    assert_eq!(out.code, 0);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let versions: Vec<&str> = v["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(versions, ["0.3.0", "0.3.1", "0.4.0", "1.0.0", "1.1.0"]);
}

#[test]
fn cni_01_pod_without_rules_is_left_alone() {
    let host = Fake {
        annotations: Some(BTreeMap::new()),
        ..Fake::default()
    };
    let out = run(&invocation("ADD"), &host);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(serde_json::from_str::<Value>(&out.stdout).unwrap(), prev());
    assert!(host.applied.lock().unwrap().is_empty());
}

#[test]
fn cni_01_annotated_pod_gets_the_rules_of_mimic_init() {
    let host = Fake {
        annotations: Some(rules(json!({
            "MIMIC_INBOUND_PORTS": "8080",
            "MIMIC_EXCLUDE_OUTBOUND_CIDRS": "10.96.0.1/32",
            "MIMIC_EXCLUDE_OUTBOUND_PORTS": "5432",
            "MIMIC_MESH": "auto",
            "MIMIC_IPV6": "true",
            "MIMIC_IPTABLES": "iptables-legacy"
        }))),
        ..Fake::default()
    };
    let out = run(&invocation("ADD"), &host);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(serde_json::from_str::<Value>(&out.stdout).unwrap(), prev());
    let applied = host.applied.lock().unwrap();
    assert_eq!(applied.len(), 1);
    let (netns, s) = &applied[0];
    assert_eq!(netns, "/var/run/netns/cni-1");
    assert_eq!(
        s,
        &Settings {
            inbound_ports: "8080".into(),
            exclude_outbound_cidrs: "10.96.0.1/32".into(),
            exclude_outbound_ports: "5432".into(),
            mesh: "auto".into(),
            ipv6: true,
            iptables: "iptables-legacy".into(),
            pod_ip: Some("10.244.1.7".into()),
            ..Settings::default()
        }
    );
}

/// Erreur au format CNI, code de sortie non nul.
fn assert_cni_error(out: &mimic_cni::Outcome, needle: &str) {
    assert_ne!(out.code, 0);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["cniVersion"], "1.0.0");
    assert!(v["code"].as_u64().is_some_and(|c| c >= 100), "{v}");
    let text = format!("{} {}", v["msg"], v["details"]);
    assert!(text.contains(needle), "{v}");
}

#[test]
fn cni_01_errors_are_reported_in_cni_format() {
    // Pod introuvable.
    assert_cni_error(&run(&invocation("ADD"), &Fake::default()), "not found");
    // Annotation illisible, puis variable inconnue.
    for (bad, needle) in [
        (json!("pas un objet"), RULES_ANNOTATION),
        (json!({"MIMIC_OOPS": "1"}), "MIMIC_OOPS"),
    ] {
        let host = Fake {
            annotations: Some(rules(bad)),
            ..Fake::default()
        };
        assert_cni_error(&run(&invocation("ADD"), &host), needle);
    }
    // Règles qui échouent.
    let host = Fake {
        annotations: Some(rules(json!({"MIMIC_INBOUND_PORTS": "*"}))),
        fail_apply: true,
        ..Fake::default()
    };
    assert_cni_error(&run(&invocation("ADD"), &host), "iptables");
    // Sans namespace réseau.
    let host = Fake {
        annotations: Some(rules(json!({}))),
        ..Fake::default()
    };
    let mut inv = invocation("ADD");
    inv.netns = None;
    assert_cni_error(&run(&inv, &host), "CNI_NETNS");
}

#[test]
fn cni_01_del_and_check_do_nothing() {
    let host = Fake::default();
    for command in ["DEL", "CHECK"] {
        let out = run(&invocation(command), &host);
        assert_eq!(out.code, 0, "{command}");
        assert!(out.stdout.is_empty(), "{command} : {}", out.stdout);
    }
    assert!(host.applied.lock().unwrap().is_empty());
}
