//! Mutation des pods par le webhook d'injection (spec §9.4, INJ-01 à 07, INJ-10 à 13, INJ-21).

use k8s_openapi::api::core::v1::{Container, EnvVar, Pod};
use mimic_injector::config::InjectorConfig;
use mimic_injector::inject::{Outcome, mesh_warning, mutate};
use serde_json::json;

fn config() -> InjectorConfig {
    serde_yaml::from_str(
        r#"
image: ghcr.io/thaliph/mimic:0.1.0
exclude_outbound_cidrs: [10.96.0.1/32]
sidecar_config: |
  mode: record
  store: { dir: /var/lib/mimic }
"#,
    )
    .unwrap()
}

fn pod(extra: serde_json::Value) -> Pod {
    let mut v = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "generateName": "checkout-7d9f-",
            "labels": {"app.kubernetes.io/name": "checkout"}
        },
        "spec": {"containers": [{
            "name": "app",
            "image": "registry.example.com/team/checkout:1.4.2",
            "ports": [{"containerPort": 8080}, {"containerPort": 9090, "protocol": "UDP"}]
        }]}
    });
    merge(&mut v, extra);
    serde_json::from_value(v).unwrap()
}

fn merge(a: &mut serde_json::Value, b: serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

fn injected(pod: &Pod) -> Pod {
    match mutate(pod, &config()) {
        Outcome::Injected(p) => *p,
        Outcome::Skipped(reason) => panic!("non injecté : {reason}"),
        Outcome::Unchanged => panic!("non injecté : déjà injecté"),
    }
}

fn env<'a>(c: &'a Container, name: &str) -> Option<&'a EnvVar> {
    c.env.iter().flatten().find(|e| e.name == name)
}

fn env_value<'a>(c: &'a Container, name: &str) -> Option<&'a str> {
    env(c, name).and_then(|e| e.value.as_deref())
}

#[test]
fn inj_01_pod_receives_init_container_and_native_sidecar() {
    let p = injected(&pod(json!({})));
    let spec = p.spec.as_ref().unwrap();
    let inits = spec.init_containers.as_ref().unwrap();
    assert_eq!(inits[0].name, "mimic-init");
    assert_eq!(env_value(&inits[0], "MIMIC_INBOUND_PORTS"), Some("8080"));
    assert_eq!(env_value(&inits[0], "MIMIC_MESH"), Some("auto"));
    // IP du pod pour le trajet entrant ambient (spec §3.3.2).
    let pod_ip = inits[0]
        .env
        .iter()
        .flatten()
        .find(|e| e.name == "POD_IP")
        .and_then(|e| e.value_from.as_ref())
        .and_then(|v| v.field_ref.as_ref())
        .map(|f| f.field_path.as_str());
    assert_eq!(pod_ip, Some("status.podIP"));
    assert_eq!(
        env_value(&inits[0], "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
        Some("10.96.0.1/32")
    );
    let caps = inits[0]
        .security_context
        .as_ref()
        .unwrap()
        .capabilities
        .as_ref()
        .unwrap();
    assert_eq!(
        caps.add.as_deref().unwrap(),
        ["NET_ADMIN".to_string(), "NET_RAW".to_string()]
    );

    let sidecar = &inits[1];
    assert_eq!(sidecar.name, "mimic");
    assert_eq!(sidecar.restart_policy.as_deref(), Some("Always"));
    assert_eq!(
        sidecar.image.as_deref(),
        Some("ghcr.io/thaliph/mimic:0.1.0")
    );
    let sc = sidecar.security_context.as_ref().unwrap();
    assert_eq!(sc.run_as_user, Some(1338));
    assert_eq!(sc.run_as_group, Some(1338));
    assert_eq!(sc.run_as_non_root, Some(true));
    let startup = sidecar.startup_probe.as_ref().unwrap();
    assert_eq!(
        startup.http_get.as_ref().unwrap().path.as_deref(),
        Some("/readyz")
    );
    assert_eq!(sidecar.ports.as_ref().unwrap()[0].container_port, 15100);
    assert_eq!(env_value(sidecar, "MIMIC_WORKLOAD"), Some("checkout"));
    assert!(
        env_value(sidecar, "MIMIC_CONFIG_YAML")
            .unwrap()
            .contains("mode: record")
    );
    assert!(env(sidecar, "POD_NAME").unwrap().value_from.is_some());
    let salt = env(sidecar, "MIMIC_REDACT_SALT").unwrap();
    let secret = salt
        .value_from
        .as_ref()
        .unwrap()
        .secret_key_ref
        .as_ref()
        .unwrap();
    assert_eq!(secret.name, "checkout-mimic");
    assert_eq!(secret.optional, Some(true));
    assert!(env(sidecar, "MIMIC_TLS_INTERCEPT").is_none());

    let volumes: Vec<&str> = spec
        .volumes
        .iter()
        .flatten()
        .map(|v| v.name.as_str())
        .collect();
    for v in [
        "mimic-store",
        "mimic-token",
        "mimic-controller-ca",
        "mimic-ca",
    ] {
        assert!(volumes.contains(&v), "volume {v} absent : {volumes:?}");
    }
    let token = spec
        .volumes
        .iter()
        .flatten()
        .find(|v| v.name == "mimic-token")
        .unwrap();
    let sa = token.projected.as_ref().unwrap().sources.as_ref().unwrap()[0]
        .service_account_token
        .as_ref()
        .unwrap();
    assert_eq!(sa.audience.as_deref(), Some("mimic-controller"));

    let meta = &p.metadata;
    assert_eq!(
        meta.annotations.as_ref().unwrap()["mimic.platform.io/status"],
        "injected"
    );
    assert_eq!(
        meta.labels.as_ref().unwrap()["mimic.platform.io/sidecar"],
        "true"
    );
    // L'application elle-même n'est pas touchée sans interception TLS.
    assert_eq!(spec.containers[0].env, None);
    assert_eq!(spec.containers[0].volume_mounts, None);
}

#[test]
fn inj_01_without_tcp_ports_all_inbound_is_intercepted() {
    let mut p = pod(json!({}));
    p.spec.as_mut().unwrap().containers[0].ports = None;
    let p = injected(&p);
    let init = &p.spec.unwrap().init_containers.unwrap()[0];
    assert_eq!(env_value(init, "MIMIC_INBOUND_PORTS"), Some("*"));
}

#[test]
fn inj_02_already_injected_pod_is_left_alone() {
    let once = injected(&pod(json!({})));
    // Réinvocation par l'API server : ni patch, ni avertissement « non injecté ».
    assert!(matches!(mutate(&once, &config()), Outcome::Unchanged));
}

#[test]
fn inj_03_opt_out_and_host_network_are_skipped() {
    let cases = [
        pod(json!({"metadata": {"annotations": {"mimic.platform.io/inject": "false"}}})),
        pod(json!({"spec": {"hostNetwork": true}})),
    ];
    for p in cases {
        match mutate(&p, &config()) {
            Outcome::Skipped(reason) => assert!(!reason.is_empty()),
            Outcome::Injected(_) => panic!("pod injecté à tort : {:?}", p.metadata),
            Outcome::Unchanged => panic!("pod pris pour déjà injecté : {:?}", p.metadata),
        }
    }
}

#[test]
fn inj_04_pod_without_workload_name_is_skipped() {
    let mut p = pod(json!({}));
    p.metadata.labels = None;
    match mutate(&p, &config()) {
        Outcome::Skipped(reason) => assert!(reason.contains("workload"), "{reason}"),
        Outcome::Injected(_) => panic!("pod injecté sans workload"),
        Outcome::Unchanged => panic!("pod pris pour déjà injecté"),
    }
}

#[test]
fn inj_04_workload_name_precedence() {
    let p = pod(json!({"metadata": {
        "annotations": {"mimic.platform.io/workload": "shop-checkout"},
        "labels": {"app": "other"}
    }}));
    let p = injected(&p);
    let sidecar = &p.spec.unwrap().init_containers.unwrap()[1];
    assert_eq!(env_value(sidecar, "MIMIC_WORKLOAD"), Some("shop-checkout"));

    let mut p = pod(json!({"metadata": {"labels": {"app": "legacy"}}}));
    p.metadata
        .labels
        .as_mut()
        .unwrap()
        .remove("app.kubernetes.io/name");
    let p = injected(&p);
    let sidecar = &p.spec.unwrap().init_containers.unwrap()[1];
    assert_eq!(env_value(sidecar, "MIMIC_WORKLOAD"), Some("legacy"));
}

#[test]
fn inj_05_tls_intercept_wires_the_ca_into_the_app() {
    let mut p = pod(json!({"metadata": {"annotations": {
        "mimic.platform.io/tls-intercept": "api.stripe.com, *.salesforce.com"
    }}}));
    p.spec.as_mut().unwrap().containers[0].env = Some(vec![EnvVar {
        name: "NODE_EXTRA_CA_CERTS".into(),
        value: Some("/etc/app/ca.pem".into()),
        ..EnvVar::default()
    }]);
    let p = injected(&p);
    let spec = p.spec.unwrap();
    let sidecar = &spec.init_containers.as_ref().unwrap()[1];
    assert_eq!(
        env_value(sidecar, "MIMIC_TLS_INTERCEPT"),
        Some("api.stripe.com,*.salesforce.com")
    );
    let app = &spec.containers[0];
    assert_eq!(
        env_value(app, "SSL_CERT_FILE"),
        Some("/var/run/mimic/ca/ca-bundle.crt")
    );
    assert_eq!(
        env_value(app, "REQUESTS_CA_BUNDLE"),
        Some("/var/run/mimic/ca/ca-bundle.crt")
    );
    assert_eq!(
        env_value(app, "CURL_CA_BUNDLE"),
        Some("/var/run/mimic/ca/ca-bundle.crt")
    );
    // Variable déjà définie par l'application : gardée.
    assert_eq!(
        env_value(app, "NODE_EXTRA_CA_CERTS"),
        Some("/etc/app/ca.pem")
    );
    let mount = app
        .volume_mounts
        .iter()
        .flatten()
        .find(|m| m.name == "mimic-ca")
        .unwrap();
    assert_eq!(mount.mount_path, "/var/run/mimic/ca");
    assert_eq!(mount.read_only, Some(true));
}

#[test]
fn inj_06_annotations_override_ports_exclusions_and_config() {
    let p = pod(json!({"metadata": {"annotations": {
        "mimic.platform.io/inbound-ports": "8080,8443",
        "mimic.platform.io/exclude-outbound-cidrs": "10.0.0.0/8",
        "mimic.platform.io/exclude-outbound-ports": "5432",
        "mimic.platform.io/config": "checkout-mimic"
    }}}));
    let p = injected(&p);
    let spec = p.spec.unwrap();
    let inits = spec.init_containers.as_ref().unwrap();
    assert_eq!(
        env_value(&inits[0], "MIMIC_INBOUND_PORTS"),
        Some("8080,8443")
    );
    assert_eq!(
        env_value(&inits[0], "MIMIC_EXCLUDE_OUTBOUND_CIDRS"),
        Some("10.96.0.1/32,10.0.0.0/8")
    );
    assert_eq!(
        env_value(&inits[0], "MIMIC_EXCLUDE_OUTBOUND_PORTS"),
        Some("5432")
    );
    let sidecar = &inits[1];
    assert!(env(sidecar, "MIMIC_CONFIG_YAML").is_none());
    assert_eq!(
        sidecar.args.as_deref().unwrap(),
        ["--config".to_string(), "/etc/mimic/mimic.yaml".to_string()]
    );
    let vol = spec
        .volumes
        .iter()
        .flatten()
        .find(|v| v.name == "mimic-config")
        .unwrap();
    assert_eq!(vol.config_map.as_ref().unwrap().name, "checkout-mimic");
}

#[test]
fn inj_07_existing_init_containers_run_after_mimic() {
    let mut p = pod(json!({}));
    p.spec.as_mut().unwrap().init_containers = Some(vec![
        Container {
            name: "migrate".into(),
            ..Container::default()
        },
        Container {
            name: "warmup".into(),
            ..Container::default()
        },
    ]);
    let p = injected(&p);
    let names: Vec<String> = p
        .spec
        .unwrap()
        .init_containers
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["mimic-init", "mimic", "migrate", "warmup"]);
}

/// Chemin `fieldRef` d'une variable posée par l'API downward.
fn field_path<'a>(c: &'a Container, name: &str) -> Option<&'a str> {
    env(c, name)?
        .value_from
        .as_ref()?
        .field_ref
        .as_ref()
        .map(|f| f.field_path.as_str())
}

fn sidecar_of(p: &Pod) -> Container {
    p.spec
        .as_ref()
        .unwrap()
        .init_containers
        .as_ref()
        .unwrap()
        .iter()
        .find(|c| c.name == "mimic")
        .unwrap()
        .clone()
}

#[test]
fn inj_10_version_labels_reach_the_sidecar() {
    let s = sidecar_of(&injected(&pod(json!({}))));
    assert_eq!(
        field_path(&s, "MIMIC_VERSION"),
        Some("metadata.labels['app.kubernetes.io/version']")
    );
    assert_eq!(
        field_path(&s, "MIMIC_POD_TEMPLATE_HASH"),
        Some("metadata.labels['pod-template-hash']")
    );
    let mut cfg = config();
    cfg.version_label = "app.example.com/release".into();
    let s = match mutate(&pod(json!({})), &cfg) {
        Outcome::Injected(p) => sidecar_of(&p),
        Outcome::Skipped(r) => panic!("{r}"),
        Outcome::Unchanged => panic!("déjà injecté"),
    };
    assert_eq!(
        field_path(&s, "MIMIC_VERSION"),
        Some("metadata.labels['app.example.com/release']")
    );
}

fn names(containers: Option<&Vec<Container>>) -> Vec<&str> {
    containers
        .into_iter()
        .flatten()
        .map(|c| c.name.as_str())
        .collect()
}

fn container(name: &str) -> Container {
    Container {
        name: name.into(),
        ..Container::default()
    }
}

fn native(name: &str) -> Container {
    Container {
        restart_policy: Some("Always".into()),
        ..container(name)
    }
}

/// Pod tel que le webhook d'Istio le rend : `istio-init`, puis `istio-proxy` en native sidecar.
fn istio_pod() -> Pod {
    let mut p = pod(json!({}));
    let spec = p.spec.as_mut().unwrap();
    spec.init_containers = Some(vec![
        container("istio-init"),
        Container {
            ports: Some(vec![k8s_openapi::api::core::v1::ContainerPort {
                container_port: 15090,
                ..Default::default()
            }]),
            ..native("istio-proxy")
        },
        container("migrate"),
    ]);
    p
}

/// Pod tel que le webhook de Linkerd le rend : `linkerd-init`, `linkerd-proxy` en conteneur.
fn linkerd_pod() -> Pod {
    let mut p = pod(json!({}));
    let spec = p.spec.as_mut().unwrap();
    spec.init_containers = Some(vec![container("linkerd-init"), container("migrate")]);
    spec.containers.insert(
        0,
        Container {
            ports: Some(vec![
                k8s_openapi::api::core::v1::ContainerPort {
                    container_port: 4143,
                    ..Default::default()
                },
                k8s_openapi::api::core::v1::ContainerPort {
                    container_port: 4191,
                    ..Default::default()
                },
            ]),
            ..container("linkerd-proxy")
        },
    );
    p
}

/// Commande d'une sonde `exec` du sidecar.
fn exec(probe: Option<&k8s_openapi::api::core::v1::Probe>) -> Vec<String> {
    let probe = probe.expect("sonde présente");
    assert!(probe.http_get.is_none(), "sonde HTTP dans un pod du mesh");
    probe
        .exec
        .as_ref()
        .and_then(|e| e.command.clone())
        .unwrap_or_default()
}

fn assert_mesh_sidecar(p: &Pod, mesh: &str) {
    let inits = p.spec.as_ref().unwrap().init_containers.as_ref().unwrap();
    let init = inits.iter().find(|c| c.name == "mimic-init").unwrap();
    assert_eq!(env_value(init, "MIMIC_MESH"), Some(mesh));
    assert_eq!(env_value(init, "MIMIC_INBOUND_PORTS"), Some("8080"));
    let sidecar = inits.iter().find(|c| c.name == "mimic").unwrap();
    assert_eq!(
        exec(sidecar.startup_probe.as_ref()),
        ["/usr/local/bin/mimic-proxy", "--probe", "/readyz"]
    );
    assert_eq!(
        exec(sidecar.liveness_probe.as_ref()),
        ["/usr/local/bin/mimic-proxy", "--probe", "/healthz"]
    );
    assert_eq!(
        exec(sidecar.readiness_probe.as_ref()),
        ["/usr/local/bin/mimic-proxy", "--probe", "/readyz"]
    );
}

#[test]
fn inj_11_mimic_runs_after_the_mesh_containers() {
    let p = injected(&istio_pod());
    assert_eq!(
        names(p.spec.as_ref().unwrap().init_containers.as_ref()),
        [
            "istio-init",
            "istio-proxy",
            "mimic-init",
            "mimic",
            "migrate"
        ]
    );
    assert_mesh_sidecar(&p, "istio");

    let p = injected(&linkerd_pod());
    assert_eq!(
        names(p.spec.as_ref().unwrap().init_containers.as_ref()),
        ["linkerd-init", "mimic-init", "mimic", "migrate"]
    );
    assert_mesh_sidecar(&p, "linkerd");

    // Sonde exec avec la configuration montée par l'annotation.
    let mut p = istio_pod();
    p.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert("mimic.platform.io/config".into(), "checkout-mimic".into());
    let p = injected(&p);
    let sidecar = sidecar_of(&p);
    assert_eq!(
        exec(sidecar.startup_probe.as_ref()),
        [
            "/usr/local/bin/mimic-proxy",
            "--config",
            "/etc/mimic/mimic.yaml",
            "--probe",
            "/readyz"
        ]
    );
}

#[test]
fn inj_12_reinvocation_adapts_a_pod_meshed_after_mimic() {
    let once = injected(&pod(json!({})));
    let istio_init = container("istio-init");
    let istio_proxy = native("istio-proxy");

    // Istio a ajouté ses conteneurs en tête : seuls MIMIC_MESH et les sondes changent.
    let mut front = once.clone();
    let inits = front
        .spec
        .as_mut()
        .unwrap()
        .init_containers
        .as_mut()
        .unwrap();
    inits.insert(0, istio_proxy.clone());
    inits.insert(0, istio_init.clone());
    let adapted = injected(&front);
    assert_eq!(
        names(adapted.spec.as_ref().unwrap().init_containers.as_ref()),
        ["istio-init", "istio-proxy", "mimic-init", "mimic"]
    );
    assert_mesh_sidecar(&adapted, "istio");
    assert!(
        matches!(mutate(&adapted, &config()), Outcome::Unchanged),
        "pod déjà adapté : aucun patch"
    );

    // Derrière ceux de Mimic : ils repassent devant.
    let mut behind = once.clone();
    let inits = behind
        .spec
        .as_mut()
        .unwrap()
        .init_containers
        .as_mut()
        .unwrap();
    inits.push(istio_init);
    inits.push(istio_proxy);
    let adapted = injected(&behind);
    assert_eq!(
        names(adapted.spec.as_ref().unwrap().init_containers.as_ref()),
        ["istio-init", "istio-proxy", "mimic-init", "mimic"]
    );
    assert_mesh_sidecar(&adapted, "istio");
    assert!(matches!(mutate(&adapted, &config()), Outcome::Unchanged));
}

#[test]
fn inj_13_mesh_annotation() {
    let annotated = |mut p: Pod, value: &str| {
        p.metadata
            .annotations
            .get_or_insert_with(Default::default)
            .insert("mimic.platform.io/mesh".into(), value.into());
        p
    };
    let init_env = |p: &Pod| {
        let inits = p.spec.as_ref().unwrap().init_containers.as_ref().unwrap();
        env_value(
            inits.iter().find(|c| c.name == "mimic-init").unwrap(),
            "MIMIC_MESH",
        )
        .map(String::from)
    };

    let ambient = annotated(pod(json!({})), "istio-ambient");
    let p = injected(&ambient);
    assert_eq!(init_env(&p).as_deref(), Some("istio-ambient"));
    // Pas de conteneur du mesh : les sondes HTTP passent par ztunnel (§3.3.4).
    assert!(sidecar_of(&p).startup_probe.unwrap().http_get.is_some());
    assert_eq!(mesh_warning(&ambient), None);

    let p = injected(&annotated(istio_pod(), "none"));
    assert_eq!(init_env(&p).as_deref(), Some("none"));

    let unknown = annotated(pod(json!({})), "consul");
    let p = injected(&unknown);
    assert_eq!(init_env(&p).as_deref(), Some("auto"));
    let warning = mesh_warning(&unknown).expect("avertissement");
    assert!(warning.contains("consul"), "{warning}");
}

/// Seuils d'une sonde : chemin (HTTP ou `exec`), période, échecs tolérés.
fn thresholds(probe: Option<&k8s_openapi::api::core::v1::Probe>) -> (String, i32, i32) {
    let probe = probe.expect("sonde présente");
    let path = match (&probe.http_get, &probe.exec) {
        (Some(h), _) => h.path.clone().unwrap_or_default(),
        (None, Some(e)) => e
            .command
            .clone()
            .unwrap_or_default()
            .last()
            .cloned()
            .unwrap(),
        (None, None) => panic!("sonde sans action"),
    };
    (
        path,
        probe.period_seconds.unwrap(),
        probe.failure_threshold.unwrap(),
    )
}

/// La `startupProbe` du sidecar attend la barrière de démarrage (`/readyz`) pendant
/// `share.startup.timeout` + 30 s ; la vivacité reste sur `/healthz` (INJ-21, spec §5.5.5).
#[test]
fn inj_21_startup_probe_waits_for_the_startup_gate() {
    let sidecar = sidecar_of(&injected(&pod(json!({}))));
    assert_eq!(
        thresholds(sidecar.startup_probe.as_ref()),
        ("/readyz".into(), 1, 60)
    );
    assert_eq!(
        thresholds(sidecar.readiness_probe.as_ref()),
        ("/readyz".into(), 10, 3)
    );
    assert_eq!(
        thresholds(sidecar.liveness_probe.as_ref()),
        ("/healthz".into(), 10, 3)
    );

    // Délai lu dans la configuration en ligne des sidecars.
    let mut cfg = config();
    cfg.sidecar_config = "mode: record\nshare:\n  url: https://ctl:8443\n  ca_file: /ca.crt\n  startup: {timeout: 90, fallback: fail}\n".into();
    let p = match mutate(&pod(json!({})), &cfg) {
        Outcome::Injected(p) => *p,
        Outcome::Skipped(r) => panic!("{r}"),
        Outcome::Unchanged => panic!("déjà injecté"),
    };
    assert_eq!(
        thresholds(sidecar_of(&p).startup_probe.as_ref()),
        ("/readyz".into(), 1, 120)
    );

    // Configuration montée par ConfigMap, que le webhook ne lit pas : délai maximal.
    let mounted = pod(json!({"metadata": {"annotations": {
        "mimic.platform.io/config": "checkout-mimic"
    }}}));
    assert_eq!(
        thresholds(sidecar_of(&injected(&mounted)).startup_probe.as_ref()),
        ("/readyz".into(), 1, 330)
    );

    // Dans un mesh, sondes `exec` avec les mêmes seuils, y compris après réinvocation.
    let meshed = sidecar_of(&injected(&istio_pod()));
    assert_eq!(
        thresholds(meshed.startup_probe.as_ref()),
        ("/readyz".into(), 1, 60)
    );
    let mut later = injected(&pod(json!({})));
    later
        .spec
        .as_mut()
        .unwrap()
        .init_containers
        .as_mut()
        .unwrap()
        .insert(0, native("istio-proxy"));
    let adapted = sidecar_of(&injected(&later));
    assert!(adapted.startup_probe.as_ref().unwrap().exec.is_some());
    assert_eq!(
        thresholds(adapted.startup_probe.as_ref()),
        ("/readyz".into(), 1, 60)
    );
    assert_eq!(
        thresholds(adapted.liveness_probe.as_ref()),
        ("/healthz".into(), 10, 3)
    );
}

/// Bodies non analysables gardés pour ces hôtes : annotation reprise par le sidecar (RED-10).
#[test]
fn red_10_raw_bodies_annotation_reaches_the_sidecar() {
    let p = injected(&pod(json!({"metadata": {"annotations": {
        "mimic.platform.io/capture-raw-bodies": "api.partner.io, inbound"
    }}})));
    assert_eq!(
        env_value(&sidecar_of(&p), "MIMIC_REDACT_RAW_BODIES"),
        Some("api.partner.io,inbound")
    );
    let p = injected(&pod(json!({})));
    assert_eq!(env_value(&sidecar_of(&p), "MIMIC_REDACT_RAW_BODIES"), None);
}

/// Variante CNI : pas de `mimic-init`, réglages en annotation, rien de privilégié (INJ-23).
#[test]
fn inj_23_cni_mode_replaces_mimic_init_with_an_annotation() {
    let mut cni = config();
    cni.cni = true;
    for p in [
        pod(json!({})),
        pod(json!({"metadata": {"annotations": {
            "mimic.platform.io/inbound-ports": "8080, nope",
            "mimic.platform.io/exclude-outbound-cidrs": "10.0.0.0/8",
            "mimic.platform.io/exclude-outbound-ports": "5432"
        }}})),
        istio_pod(),
    ] {
        let classic = injected(&p);
        let Outcome::Injected(with_cni) = mutate(&p, &cni) else {
            panic!("non injecté");
        };
        let spec = with_cni.spec.as_ref().unwrap();
        let inits = spec.init_containers.as_deref().unwrap_or_default();
        assert!(
            inits.iter().all(|c| c.name != "mimic-init"),
            "{:?}",
            names(Some(&inits.to_vec()))
        );
        for c in inits.iter().chain(spec.containers.iter()) {
            let sc = c.security_context.as_ref();
            assert_ne!(
                sc.and_then(|s| s.run_as_user),
                Some(0),
                "{} en root",
                c.name
            );
            let added = sc
                .and_then(|s| s.capabilities.as_ref())
                .and_then(|c| c.add.clone())
                .unwrap_or_default();
            assert!(added.is_empty(), "{} : capacités {added:?}", c.name);
        }
        // Mêmes réglages que ceux que mimic-init aurait reçus.
        let init = classic
            .spec
            .as_ref()
            .unwrap()
            .init_containers
            .iter()
            .flatten()
            .find(|c| c.name == "mimic-init")
            .unwrap();
        let expected: serde_json::Map<String, serde_json::Value> = init
            .env
            .iter()
            .flatten()
            .filter(|e| e.name.starts_with("MIMIC_"))
            .map(|e| (e.name.clone(), json!(e.value.clone().unwrap_or_default())))
            .collect();
        let annotation = with_cni
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("mimic.platform.io/cni-rules"))
            .expect("annotation cni-rules");
        let got: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(annotation).unwrap();
        assert_eq!(got, expected);
        // Le sidecar et les volumes ne changent pas.
        assert_eq!(sidecar_of(&with_cni), sidecar_of(&classic));
        assert_eq!(spec.volumes, classic.spec.as_ref().unwrap().volumes);
    }
    // Sans CNI : pas d'annotation.
    let classic = injected(&pod(json!({})));
    assert!(
        classic
            .metadata
            .annotations
            .as_ref()
            .is_none_or(|a| !a.contains_key("mimic.platform.io/cni-rules"))
    );
}
