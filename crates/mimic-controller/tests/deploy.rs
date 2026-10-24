//! Les manifests du contrôleur restent cohérents avec le code.

use mimic_controller::config::{AuthConfig, ControllerConfig};

#[test]
fn crd_manifest_matches_the_code() {
    let file = include_str!("../../../deploy/controller/crd.yaml");
    let body: String = file
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(
        body,
        mimic_controller::crd::crd_yaml(),
        "régénérer : mimic-controller crd > deploy/controller/crd.yaml"
    );
}

#[test]
fn controller_configmap_is_valid() {
    let docs = include_str!("../../../deploy/controller/controller.yaml");
    let cm: serde_yaml::Value = serde_yaml::Deserializer::from_str(docs)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .find(|d: &serde_yaml::Value| d["kind"] == "ConfigMap")
        .unwrap();
    let config: ControllerConfig =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    assert!(matches!(config.auth, AuthConfig::Kubernetes));
    assert!(config.ui_listen.ip().is_loopback());
}

fn docs(text: &str) -> Vec<serde_yaml::Value> {
    serde_yaml::Deserializer::from_str(text)
        .map(|d| serde::Deserialize::deserialize(d).unwrap())
        .collect()
}

/// Le contrôleur lit les labels des namespaces (garde-fou production, CRD-07).
#[test]
fn crd_07_controller_may_watch_namespaces() {
    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));
    let role = rbac
        .iter()
        .find(|d| d["kind"] == "ClusterRole" && d["metadata"]["name"] == "mimic-controller")
        .unwrap();
    let rule = role["rules"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|r| r["resources"][0] == "namespaces")
        .expect("droit sur les namespaces");
    for verb in ["get", "list", "watch"] {
        assert!(
            rule["verbs"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|v| v == verb),
            "{verb}"
        );
    }
}

/// La politique d'admission refuse les mêmes modes, sur le même label, que le contrôleur.
#[test]
fn crd_07_admission_policy_matches_the_guard() {
    use mimic_controller::reconcile::{PRODUCTION_LABEL, is_mock};
    let policy = docs(include_str!(
        "../../../deploy/controller/production-guard.yaml"
    ));
    let vap = policy
        .iter()
        .find(|d| d["kind"] == "ValidatingAdmissionPolicy")
        .unwrap();
    let expr = vap["spec"]["validations"][0]["expression"]
        .as_str()
        .unwrap();
    for mode in mimic_controller::crd::MODES {
        assert_eq!(expr.contains(&format!("'{mode}'")), is_mock(mode), "{mode}");
    }
    assert!(expr.contains(&format!("'{}'", PRODUCTION_LABEL.0)));
    assert!(expr.contains(&format!("'{}'", PRODUCTION_LABEL.1)));
    let rule = &vap["spec"]["matchConstraints"]["resourceRules"][0];
    assert_eq!(rule["resources"][0], "mimicworkloads");
    let binding = policy
        .iter()
        .find(|d| d["kind"] == "ValidatingAdmissionPolicyBinding")
        .unwrap();
    assert_eq!(binding["spec"]["policyName"], vap["metadata"]["name"]);
}

/// Historique court par défaut (CTL-21) : 24 heures, dans le code comme dans le manifest.
#[test]
fn ctl_21_retention_defaults_to_24_hours() {
    let day = std::time::Duration::from_secs(24 * 3600);
    let defaults = ControllerConfig::default();
    assert_eq!(defaults.retention, day);
    assert_eq!(defaults.max_exemplars_per_workload, 10_000);
    assert_eq!(defaults.max_exemplar_bytes_per_workload, 64 << 20);
    assert_eq!(defaults.max_exemplar_bytes_total, 512 << 20);
    let cm = docs(include_str!("../../../deploy/controller/controller.yaml"))
        .into_iter()
        .find(|d| d["kind"] == "ConfigMap")
        .unwrap();
    let text = cm["data"]["controller.yaml"].as_str().unwrap();
    let raw: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
    assert_eq!(raw["retention"].as_u64(), Some(86_400));
    assert_eq!(raw["max_exemplars_per_workload"].as_u64(), Some(10_000));
    assert_eq!(
        raw["max_exemplar_bytes_per_workload"].as_u64(),
        Some(64 << 20)
    );
    assert_eq!(raw["max_exemplar_bytes_total"].as_u64(), Some(512 << 20));
    let config: ControllerConfig = serde_yaml::from_str(text).unwrap();
    assert_eq!(config.retention, day);
}

fn find<'a>(docs: &'a [serde_yaml::Value], kind: &str, name: &str) -> &'a serde_yaml::Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("{kind} {name} absent"))
}

fn verbs(rule: &serde_yaml::Value) -> Vec<String> {
    rule["verbs"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn rule_on<'a>(role: &'a serde_yaml::Value, resource: &str) -> Option<&'a serde_yaml::Value> {
    role["rules"].as_sequence().unwrap().iter().find(|r| {
        r["resources"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|x| x == resource)
    })
}

/// Haute disponibilité (CTL-28, spec §5.5.4) : deux réplicas, une seule routée.
#[test]
fn ctl_28_controller_runs_two_replicas_behind_the_active_one() {
    use mimic_controller::leader::LEADER_LABEL;
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let deploy = find(&manifests, "Deployment", "mimic-controller");
    let spec = &deploy["spec"];
    assert_eq!(spec["replicas"].as_u64(), Some(2));
    assert_eq!(spec["strategy"]["type"], "RollingUpdate");
    assert_eq!(
        spec["strategy"]["rollingUpdate"]["maxUnavailable"].as_u64(),
        Some(0)
    );
    let pod = &spec["template"]["spec"];
    let anti =
        &pod["affinity"]["podAntiAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"][0];
    assert_eq!(
        anti["podAffinityTerm"]["topologyKey"],
        "kubernetes.io/hostname"
    );
    assert!(
        pod["affinity"]["podAntiAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
            .is_null(),
        "anti-affinité préférentielle : un nœud unique reste possible"
    );
    let container = &pod["containers"][0];
    let env = container["env"].as_sequence().expect("env du contrôleur");
    let pod_name = env
        .iter()
        .find(|e| e["name"] == "POD_NAME")
        .expect("POD_NAME");
    assert_eq!(
        pod_name["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.name"
    );
    assert!(container["livenessProbe"]["httpGet"]["path"].is_string());

    // Le Service ne route que vers la réplique marquée.
    let svc = find(&manifests, "Service", "mimic-controller");
    assert_eq!(svc["spec"]["selector"][LEADER_LABEL.0], LEADER_LABEL.1);
    assert_eq!(
        svc["spec"]["selector"]["app.kubernetes.io/name"],
        "mimic-controller"
    );

    let pdb = find(&manifests, "PodDisruptionBudget", "mimic-controller");
    assert_eq!(pdb["spec"]["minAvailable"].as_u64(), Some(1));
    assert_eq!(
        pdb["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"],
        "mimic-controller"
    );

    let cm = find(&manifests, "ConfigMap", "mimic-controller");
    let config: ControllerConfig =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    let election = config.leader_election.expect("leader_election");
    assert_eq!(election.lease, "mimic-controller");
    assert_eq!(election.namespace, "mimic-system");
    assert_eq!(election.identity, None, "identité lue dans POD_NAME");

    // Droits : bail et marque dans mimic-system seulement.
    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));
    let role = find(&rbac, "Role", "mimic-controller");
    assert_eq!(role["metadata"]["namespace"], "mimic-system");
    let leases = rule_on(role, "leases").expect("droit sur les leases");
    assert_eq!(leases["apiGroups"][0], "coordination.k8s.io");
    let lease_verbs: Vec<String> = role["rules"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter(|r| {
            r["resources"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|x| x == "leases")
        })
        .flat_map(verbs)
        .collect();
    for verb in ["get", "create", "patch"] {
        assert!(lease_verbs.iter().any(|v| v == verb), "leases : {verb}");
    }
    let pods = rule_on(role, "pods").expect("droit sur les pods");
    for verb in ["list", "patch"] {
        assert!(verbs(pods).iter().any(|v| v == verb), "pods : {verb}");
    }
    let binding = find(&rbac, "RoleBinding", "mimic-controller");
    assert_eq!(binding["metadata"]["namespace"], "mimic-system");
    assert_eq!(binding["roleRef"]["kind"], "Role");
    assert_eq!(binding["subjects"][0]["name"], "mimic-controller");
    let cluster = find(&rbac, "ClusterRole", "mimic-controller");
    assert!(rule_on(cluster, "leases").is_none());
    let cluster_pods = rule_on(cluster, "pods").unwrap();
    assert_eq!(
        verbs(cluster_pods),
        ["get"],
        "aucune écriture sur les pods du cluster"
    );
}

/// Le schéma de la CRD livrée n'accepte que `relay` et `refuse` pour `spec.mock.websocket`
/// (CRD-10).
#[test]
fn crd_10_schema_restricts_mock_websocket() {
    let crd = docs(include_str!("../../../deploy/controller/crd.yaml"));
    let schema = &crd[0]["spec"]["versions"][0]["schema"]["openAPIV3Schema"];
    let websocket = &schema["properties"]["spec"]["properties"]["mock"]["properties"]["websocket"];
    let values: Vec<&str> = websocket["enum"]
        .as_sequence()
        .expect("enum de spec.mock.websocket")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(values, ["relay", "refuse"]);
    assert_eq!(mimic_controller::crd::MOCK_WEBSOCKETS, ["relay", "refuse"]);
}

/// Manifests de production du contrôleur (CTL-35) et collecte de ses métriques (CTL-32).
#[test]
fn ctl_35_production_manifests() {
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let deploy = find(&manifests, "Deployment", "mimic-controller");
    let pod = &deploy["spec"]["template"]["spec"];
    assert_eq!(pod["containers"][0]["image"], "ghcr.io/thaliph/mimic:0.1.0");
    for key in ["runAsUser", "runAsGroup", "fsGroup"] {
        assert_eq!(pod["securityContext"][key].as_u64(), Some(1338), "{key}");
    }
    assert_eq!(pod["priorityClassName"], "mimic-control-plane");
    let memory = pod["containers"][0]["resources"]["requests"]["memory"]
        .as_str()
        .unwrap();
    let gib = memory
        .strip_suffix("Gi")
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    assert!(gib >= 1, "mémoire demandée : {memory}");

    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));
    let cli = find(&rbac, "Role", "mimic-cli");
    assert_eq!(cli["metadata"]["namespace"], "mimic-system");
    let rules = cli["rules"].as_sequence().unwrap();
    assert_eq!(rules.len(), 2, "rien d'autre : {rules:?}");
    let pods = rule_on(cli, "pods").expect("pods");
    assert_eq!(verbs(pods), ["get", "list"]);
    let forward = rule_on(cli, "pods/portforward").expect("pods/portforward");
    assert_eq!(verbs(forward), ["create"]);
}

#[test]
fn ctl_32_metrics_are_reachable_by_the_monitoring_namespace() {
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let policy = find(&manifests, "NetworkPolicy", "mimic-controller");
    let ingress = policy["spec"]["ingress"].as_sequence().unwrap();
    let monitoring = ingress
        .iter()
        .find(|rule| {
            rule["from"].as_sequence().unwrap().iter().any(|f| {
                f["namespaceSelector"]["matchLabels"]["mimic.platform.io/monitoring"] == "true"
            })
        })
        .expect("entrée depuis le namespace de monitoring");
    assert_eq!(monitoring["ports"][0]["port"].as_u64(), Some(8443));

    let scrape = docs(include_str!(
        "../../../deploy/example/vmpodscrape-controller.yaml"
    ));
    let controller = scrape
        .iter()
        .find(|d| {
            d["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"] == "mimic-controller"
        })
        .expect("collecte du contrôleur");
    assert_eq!(controller["metadata"]["namespace"], "mimic-system");
    let endpoint = &controller["spec"]["podMetricsEndpoints"][0];
    assert_eq!(endpoint["port"], "api");
    assert_eq!(endpoint["path"], "/metrics");
    assert_eq!(endpoint["scheme"], "https");
}

const PRODUCTION_DOC: &str = include_str!("../../../docs/guide/Exploiter-en-production.md");
const METRICS_DOC: &str = include_str!("../../../docs/guide/Metriques.md");
const ALERTS_DOC: &str = include_str!("../../../docs/guide/Alertes-et-supervision.md");

/// Valeurs entre accents graves d'une ligne de tableau du guide.
fn quoted(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).collect()
}

/// Ligne de tableau d'une page du guide dont la première colonne vaut `cell`, dans la
/// section `section`.
fn row<'a>(page: &'a str, section: &str, cell: &str) -> &'a str {
    let from = page
        .find(section)
        .unwrap_or_else(|| panic!("section « {section} » absente"));
    page[from..]
        .lines()
        .find(|l| l.starts_with(&format!("| {cell} |")))
        .unwrap_or_else(|| panic!("ligne « {cell} » absente de « {section} »"))
}

/// Le dimensionnement et l'UID du contrôleur donnés par le guide sont ceux du manifest
/// livré (DOC-03).
#[test]
fn doc_03_controller_sizing_matches_the_manifest() {
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let pod = &find(&manifests, "Deployment", "mimic-controller")["spec"]["template"]["spec"];
    let resources = &pod["containers"][0]["resources"];
    let request = resources["requests"]["memory"].as_str().unwrap();
    let limit = resources["limits"]["memory"].as_str().unwrap();
    let cpu = resources["requests"]["cpu"].as_str().unwrap();

    // « ### Contrôleur » du dimensionnement : la ligne mémoire ne cite que les deux valeurs
    // du manifest, dans l'ordre requête puis limite.
    let memory = row(PRODUCTION_DOC, "### Contrôleur", "mémoire");
    let (head, _) = memory.split_once(" | environ").expect("ligne mémoire");
    assert_eq!(
        quoted(head),
        [request, limit],
        "mémoire du contrôleur dans le guide : {head}"
    );
    assert_eq!(quoted(row(PRODUCTION_DOC, "### Contrôleur", "CPU"))[0], cpu);

    // Sécurité : l'UID du plan de contrôle est celui du manifest, et 1337 (istio-proxy)
    // n'apparaît dans le guide que là où il est question d'un mesh.
    let uid = pod["securityContext"]["runAsUser"].as_u64().unwrap();
    let security = row(PRODUCTION_DOC, "## Sécurité", "contrôleur et webhook");
    assert!(
        security.contains(&format!("UID et GID {uid}")),
        "UID du contrôleur dans le guide : {security}"
    );
    for (n, line) in PRODUCTION_DOC.lines().enumerate() {
        let mesh = ["istio", "linkerd", "mesh"]
            .iter()
            .any(|m| line.to_lowercase().contains(m));
        assert!(
            !line.contains("1337") || mesh,
            "Exploiter-en-production.md:{} cite l'UID 1337 hors mesh : {line}",
            n + 1
        );
    }
}

/// Le guide donne le port de collecte du contrôleur ouvert par la NetworkPolicy livrée, et
/// ne le déclare plus non collectable (DOC-04).
#[test]
fn doc_04_controller_metrics_port_matches_the_network_policy() {
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let policy = find(&manifests, "NetworkPolicy", "mimic-controller");
    let monitoring = policy["spec"]["ingress"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|rule| {
            rule["from"].as_sequence().unwrap().iter().any(|f| {
                f["namespaceSelector"]["matchLabels"]["mimic.platform.io/monitoring"] == "true"
            })
        })
        .expect("entrée depuis le namespace de monitoring");
    let port = monitoring["ports"][0]["port"].as_u64().unwrap();
    let label = "mimic.platform.io/monitoring";

    let controller = row(METRICS_DOC, "## Où les collecter", "contrôleur");
    assert!(
        controller.contains(&port.to_string()),
        "port de collecte du contrôleur dans le guide : {controller}"
    );
    assert!(
        !controller.contains("non collectable"),
        "le contrôleur est collectable : {controller}"
    );
    for page in [METRICS_DOC, ALERTS_DOC] {
        assert!(page.contains(label), "label {label} attendu");
    }

    // L'exemple de collecte livré vise ce port, par son nom dans le Deployment.
    let scrape = docs(include_str!(
        "../../../deploy/example/vmpodscrape-controller.yaml"
    ));
    let endpoint = &scrape[0]["spec"]["podMetricsEndpoints"][0];
    let name = endpoint["port"].as_str().unwrap();
    assert!(
        METRICS_DOC.contains(&format!("port `{name}` ({port})")),
        "le guide doit nommer le port de collecte « {name} » ({port})"
    );
    let ports = find(&manifests, "Deployment", "mimic-controller")["spec"]["template"]["spec"]
        ["containers"][0]["ports"]
        .as_sequence()
        .unwrap();
    assert!(
        ports
            .iter()
            .any(|p| p["name"] == name && p["containerPort"].as_u64() == Some(port)),
        "port {name} ({port}) absent du Deployment : {ports:?}"
    );
}

const RULES: &str = include_str!("../../../deploy/monitoring/rules.yaml");

/// Noms des alertes des règles livrées.
fn alerts() -> Vec<String> {
    let rules: serde_yaml::Value = serde_yaml::from_str(RULES).unwrap();
    rules["groups"]
        .as_sequence()
        .unwrap()
        .iter()
        .flat_map(|g| g["rules"].as_sequence().unwrap())
        .filter_map(|r| r["alert"].as_str().map(String::from))
        .collect()
}

/// Chaque alerte des règles livrées (`deploy/monitoring/rules.yaml`) a une ligne dans
/// « Conduite à tenir » du guide, et réciproquement (DOC-06).
#[test]
fn doc_06_every_alert_has_a_runbook_row() {
    let mut declared = alerts();
    let runbook = ALERTS_DOC
        .split_once("## Conduite à tenir")
        .expect("section « Conduite à tenir »")
        .1;
    let mut documented: Vec<&str> = runbook
        .lines()
        .filter_map(|l| l.strip_prefix("| `")?.split_once('`'))
        .map(|(name, _)| name)
        .collect();
    assert!(declared.len() > 25, "alertes : {declared:?}");
    declared.sort_unstable();
    documented.sort_unstable();
    assert_eq!(declared, documented);
}

/// La `PrometheusRule` livrée porte exactement les règles brutes testées par `promtool`, les
/// règles sur `mimic_controller_pods` agrègent par workload, et le guide renvoie aux fichiers
/// au lieu de recopier les règles (MON-06).
#[test]
fn mon_06_prometheus_rule_is_the_tested_rules() {
    let rule = docs(include_str!(
        "../../../deploy/monitoring/prometheus-rule.yaml"
    ));
    let rule = find(&rule, "PrometheusRule", "mimic");
    assert_eq!(rule["apiVersion"], "monitoring.coreos.com/v1");
    assert_eq!(rule["metadata"]["namespace"], "mimic-system");
    let raw: serde_yaml::Value = serde_yaml::from_str(RULES).unwrap();
    assert_eq!(
        rule["spec"]["groups"], raw["groups"],
        "prometheus-rule.yaml et rules.yaml divergent"
    );

    let rules: Vec<&serde_yaml::Value> = raw["groups"]
        .as_sequence()
        .unwrap()
        .iter()
        .flat_map(|g| g["rules"].as_sequence().unwrap())
        .collect();
    let pods: Vec<&str> = rules
        .iter()
        .filter_map(|r| r["expr"].as_str())
        .filter(|e| e.contains("mimic_controller_pods"))
        .collect();
    assert!(!pods.is_empty());
    for expr in pods {
        assert!(expr.contains("by (workload)"), "{expr}");
    }
    for name in [
        "MimicControllerCertificateExpiring",
        "MimicInjectorCertificateExpiring",
        "MimicStoreDiskHigh",
        "MimicPodEvicted",
        "MimicVersionSkew",
    ] {
        assert!(alerts().iter().any(|a| a == name), "{name} absente");
    }

    assert!(
        !ALERTS_DOC.contains("kind: PrometheusRule"),
        "règles recopiées"
    );
    assert!(!ALERTS_DOC.contains("- alert: "), "règles recopiées");
    for file in [
        "deploy/monitoring/rules.yaml",
        "deploy/monitoring/prometheus-rule.yaml",
    ] {
        assert!(ALERTS_DOC.contains(file), "{file} non cité par le guide");
    }
}

/// L'API agrégée de lecture est déclarée, joignable et bornée aux lectures (SEC-08).
#[test]
fn sec_08_aggregated_read_api_manifests() {
    let manifests = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let rbac = docs(include_str!("../../../deploy/controller/rbac.yaml"));

    let api = find(&manifests, "APIService", "v1alpha1.data.mimic.platform.io");
    assert_eq!(api["spec"]["group"], "data.mimic.platform.io");
    assert_eq!(api["spec"]["version"], "v1alpha1");
    assert_eq!(api["spec"]["service"]["name"], "mimic-controller");
    assert_eq!(api["spec"]["service"]["namespace"], "mimic-system");
    assert_eq!(
        api["metadata"]["annotations"]["cert-manager.io/inject-ca-from"],
        "mimic-system/mimic-controller-tls"
    );
    let port = api["spec"]["service"]["port"].as_u64().unwrap();

    let service = find(&manifests, "Service", "mimic-controller");
    let svc_port = service["spec"]["ports"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|p| p["port"].as_u64() == Some(port))
        .expect("port de l'APIService sur le Service");
    assert_eq!(svc_port["name"], "aggregated");
    assert_eq!(svc_port["targetPort"], "aggregated");
    let deployment = find(&manifests, "Deployment", "mimic-controller");
    let container = &deployment["spec"]["template"]["spec"]["containers"][0];
    let container_port = container["ports"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "aggregated")
        .expect("port aggregated du conteneur")["containerPort"]
        .as_u64()
        .unwrap();
    let policy = find(&manifests, "NetworkPolicy", "mimic-controller");
    let open = policy["spec"]["ingress"]
        .as_sequence()
        .unwrap()
        .iter()
        .any(|r| {
            r["from"]
                .as_sequence()
                .unwrap()
                .iter()
                .any(|f| f["ipBlock"]["cidr"] == "0.0.0.0/0")
                && r["ports"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .any(|p| p["port"].as_u64() == Some(container_port))
        });
    assert!(
        open,
        "NetworkPolicy : {container_port} ouvert à toute source"
    );

    // La configuration du contrôleur sert l'API agrégée sur ce port.
    let cm = manifests.iter().find(|d| d["kind"] == "ConfigMap").unwrap();
    let config: ControllerConfig =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    config.validate().unwrap();
    let aggregated = config.aggregated.expect("aggregated configuré");
    assert_eq!(u64::from(aggregated.listen.port()), container_port);
    assert!(
        aggregated.client_ca_file.is_none(),
        "CA lue dans kube-system"
    );

    // Droits du contrôleur : SubjectAccessReview, ConfigMap de kube-system.
    let role = find(&rbac, "ClusterRole", "mimic-controller");
    let sar = rule_on(role, "subjectaccessreviews").expect("subjectaccessreviews");
    assert_eq!(verbs(sar), ["create"]);
    let binding = rbac
        .iter()
        .find(|d| {
            d["kind"] == "RoleBinding"
                && d["metadata"]["namespace"] == "kube-system"
                && d["roleRef"]["name"] == "extension-apiserver-authentication-reader"
        })
        .expect("RoleBinding extension-apiserver-authentication-reader");
    assert_eq!(binding["subjects"][0]["name"], "mimic-controller");
    assert_eq!(binding["subjects"][0]["namespace"], "mimic-system");

    // Le rôle des équipes : lecture seule du groupe agrégé.
    let viewer = find(&rbac, "ClusterRole", "mimic-data-viewer");
    let rules = viewer["rules"].as_sequence().unwrap();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0]["apiGroups"][0], "data.mimic.platform.io");
    let mut resources: Vec<&str> = rules[0]["resources"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    resources.sort_unstable();
    assert_eq!(
        resources,
        [
            "workloads",
            "workloads/openapi",
            "workloads/operations",
            "workloads/requests",
            "workloads/snapshots"
        ]
    );
    let mut granted = verbs(&rules[0]);
    granted.sort_unstable();
    assert_eq!(granted, ["get", "list"]);
}

/// Correctif d'un overlay qui vise `kind`/`name`.
fn patch_for<'a>(k: &'a serde_yaml::Value, kind: &str, name: &str) -> &'a serde_yaml::Value {
    k["patches"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|p| p["target"]["kind"] == kind && p["target"]["name"] == name)
        .unwrap_or_else(|| panic!("correctif {kind} {name} absent"))
}

/// Contenu d'un correctif en ligne (`patch: |`).
fn inline(p: &serde_yaml::Value) -> serde_yaml::Value {
    serde_yaml::from_str(p["patch"].as_str().unwrap()).unwrap()
}

/// Stockage partagé par défaut, variantes S3 et `ReadWriteOnce` (CTL-50).
#[test]
fn ctl_50_storage_variants_keep_both_replicas_schedulable() {
    let base = docs(include_str!("../../../deploy/controller/controller.yaml"));
    let pvc = find(&base, "PersistentVolumeClaim", "mimic-controller");
    assert_eq!(pvc["spec"]["accessModes"][0], "ReadWriteMany");
    let deploy = find(&base, "Deployment", "mimic-controller");
    let pod = &deploy["spec"]["template"]["spec"];
    assert!(pod["affinity"]["podAntiAffinity"].is_mapping());
    let labels = &pod["affinity"]["podAntiAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"]
        [0]["podAffinityTerm"]["labelSelector"]["matchLabels"];
    assert_eq!(
        find(&base, "PodDisruptionBudget", "mimic-controller")["spec"]["minAvailable"].as_u64(),
        Some(1)
    );

    // Base kustomize : des fichiers qui existent, contrôleur compris.
    let k: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../../deploy/controller/kustomization.yaml"
    ))
    .unwrap();
    let files: Vec<&str> = k["resources"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    for f in &files {
        let path = format!("{}/../../deploy/controller/{f}", env!("CARGO_MANIFEST_DIR"));
        assert!(std::path::Path::new(&path).exists(), "{f} absent");
    }
    assert!(files.contains(&"controller.yaml"), "{files:?}");

    // ReadWriteOnce : tout sur le nœud du volume, sans PodDisruptionBudget.
    let rwo: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../../deploy/controller-rwo/kustomization.yaml"
    ))
    .unwrap();
    assert_eq!(rwo["resources"][0], "../controller");
    let pvc = inline(patch_for(&rwo, "PersistentVolumeClaim", "mimic-controller"));
    assert_eq!(pvc["spec"]["accessModes"][0], "ReadWriteOnce");
    let affinity = &inline(patch_for(&rwo, "Deployment", "mimic-controller"))["spec"]["template"]["spec"]
        ["affinity"];
    assert_eq!(affinity["podAntiAffinity"]["$patch"], "delete");
    let term = &affinity["podAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"][0];
    assert_eq!(term["topologyKey"], "kubernetes.io/hostname");
    assert_eq!(&term["labelSelector"]["matchLabels"], labels);
    let pdb = inline(patch_for(&rwo, "PodDisruptionBudget", "mimic-controller"));
    assert_eq!(pdb["$patch"], "delete");
    assert_eq!(
        deploy["spec"]["strategy"]["rollingUpdate"]["maxSurge"].as_u64(),
        Some(1),
        "la réplique du rollout rejoint le nœud du volume"
    );

    // S3 : même configuration hors `store`, plus de PVC.
    let s3: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../../deploy/controller-s3/kustomization.yaml"
    ))
    .unwrap();
    assert_eq!(s3["resources"][0], "../controller");
    let ops: serde_yaml::Value = serde_yaml::from_str(
        patch_for(&s3, "ConfigMap", "mimic-controller")["patch"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let op = &ops[0];
    assert_eq!(op["op"], "replace");
    assert_eq!(op["path"], "/data/controller.yaml");
    let text = op["value"].as_str().unwrap();
    let s3_config: ControllerConfig = serde_yaml::from_str(text).unwrap();
    s3_config.validate().unwrap();
    assert!(s3_config.store.starts_with("s3://"), "{}", s3_config.store);
    let cm = find(&base, "ConfigMap", "mimic-controller");
    let mut base_yaml: serde_yaml::Value =
        serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    let mut s3_yaml: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
    base_yaml.as_mapping_mut().unwrap().remove("store");
    s3_yaml.as_mapping_mut().unwrap().remove("store");
    assert_eq!(base_yaml, s3_yaml, "configuration S3 = défaut hors store");
    let pvc = inline(patch_for(&s3, "PersistentVolumeClaim", "mimic-controller"));
    assert_eq!(pvc["$patch"], "delete");
    let pod = &inline(patch_for(&s3, "Deployment", "mimic-controller"))["spec"]["template"]["spec"];
    let data = pod["volumes"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "data")
        .unwrap();
    assert!(data["emptyDir"].is_mapping());
    assert!(data["persistentVolumeClaim"].is_null());
    let env_from = &pod["containers"][0]["envFrom"][0]["secretRef"];
    assert_eq!(env_from["name"], "mimic-controller-s3");
    assert_eq!(env_from["optional"], true);
}
