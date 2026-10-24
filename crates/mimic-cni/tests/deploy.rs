//! Critère CNI-03 de la spec (§3.1b) : manifests de la variante CNI.

fn docs() -> Vec<serde_yaml::Value> {
    serde_yaml::Deserializer::from_str(include_str!("../../../deploy/cni/mimic-cni.yaml"))
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect()
}

#[test]
fn cni_03_daemonset_and_rights() {
    let all = docs();
    let ds = all
        .iter()
        .find(|d| d["kind"] == "DaemonSet" && d["metadata"]["name"] == "mimic-cni")
        .expect("DaemonSet mimic-cni");
    assert_eq!(ds["metadata"]["namespace"], "mimic-system");
    let pod = &ds["spec"]["template"]["spec"];
    let paths: Vec<&str> = pod["volumes"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(|v| v["hostPath"]["path"].as_str())
        .collect();
    for p in ["/opt/cni/bin", "/etc/cni/net.d"] {
        assert!(paths.contains(&p), "hostPath {p} : {paths:?}");
    }
    assert!(
        pod["tolerations"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|t| t["operator"] == "Exists" && t.get("key").is_none()),
        "tolère tous les taints"
    );
    let container = &pod["containers"][0];
    assert_eq!(container["args"][0], "install");
    let role = all
        .iter()
        .find(|d| d["kind"] == "ClusterRole" && d["metadata"]["name"] == "mimic-cni")
        .unwrap();
    let rules = role["rules"].as_sequence().unwrap();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0]["resources"][0], "pods");
    assert_eq!(rules[0]["verbs"].as_sequence().unwrap().len(), 1);
    assert_eq!(rules[0]["verbs"][0], "get");

    let dockerfile = include_str!("../../../Dockerfile");
    assert!(dockerfile.contains("-p mimic-cni"), "binaire construit");
    assert!(
        dockerfile.contains("/src/target/release/mimic-cni"),
        "binaire copié dans l'image"
    );
}
