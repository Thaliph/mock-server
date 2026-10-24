//! `mimicctl install` : manifests du socle écrits sur la sortie standard (spec §9.5b, CLI-22).
//! API attendue : `mimicctl::install::{InstallOptions, Storage, manifests}`.

use mimicctl::install::{InstallOptions, Storage, manifests};
use serde_json::Value;

const DEFAULT_IMAGE: &str = "ghcr.io/thaliph/mimic";
const DEFAULT_INIT_IMAGE: &str = "ghcr.io/thaliph/mimic-init";

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn render(options: &InstallOptions) -> String {
    manifests(options).expect("les options valides produisent des manifests")
}

/// Découpe la sortie en documents YAML (séparateur `---` seul sur sa ligne) ; un document
/// vide (en-tête de commentaires) est ignoré.
fn documents(output: &str) -> Vec<Value> {
    let mut docs = Vec::new();
    let mut current = String::new();
    for line in output.lines().chain(std::iter::once("---")) {
        if line.trim_end() == "---" {
            let v: Value = serde_yaml::from_str(&current)
                .unwrap_or_else(|e| panic!("document YAML invalide : {e}\n{current}"));
            if !v.is_null() {
                docs.push(v);
            }
            current.clear();
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    docs
}

fn kind_name(doc: &Value) -> (String, String) {
    (
        doc["kind"].as_str().unwrap_or_default().to_owned(),
        doc["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

fn find<'a>(docs: &'a [Value], kind: &str, name: &str) -> &'a Value {
    docs.iter()
        .find(|d| kind_name(d) == (kind.to_owned(), name.to_owned()))
        .unwrap_or_else(|| panic!("{kind}/{name} absent de la sortie"))
}

fn position(docs: &[Value], kind: &str, name: &str) -> usize {
    docs.iter()
        .position(|d| kind_name(d) == (kind.to_owned(), name.to_owned()))
        .unwrap_or_else(|| panic!("{kind}/{name} absent de la sortie"))
}

fn first_container_image<'a>(docs: &'a [Value], deployment: &str) -> &'a Value {
    &find(docs, "Deployment", deployment)["spec"]["template"]["spec"]["containers"][0]["image"]
}

/// Configuration du webhook (`injector.yaml` du ConfigMap `mimic-injector`).
fn injector_config(docs: &[Value]) -> Value {
    let cm = find(docs, "ConfigMap", "mimic-injector");
    let text = cm["data"]["injector.yaml"].as_str().expect("injector.yaml");
    serde_yaml::from_str(text).expect("la configuration du webhook est valide")
}

#[test]
fn cli_22_default_is_controller_then_webhook_with_default_images() {
    let out = render(&InstallOptions::default());
    let docs = documents(&out);
    assert!(
        docs.iter()
            .all(|d| d["kind"].is_string() && d["apiVersion"].is_string())
    );

    // Le socle d'abord (espace de noms, contrôleur), puis le webhook.
    let ns = position(&docs, "Namespace", "mimic-system");
    let controller = position(&docs, "Deployment", "mimic-controller");
    let injector = position(&docs, "Deployment", "mimic-injector");
    assert!(ns < controller && controller < injector);
    assert!(position(&docs, "MutatingWebhookConfiguration", "mimic-injector") > controller);

    // Variante `shared` : PVC ReadWriteMany ; ni CNI ni garde de production.
    let pvc = find(&docs, "PersistentVolumeClaim", "mimic-controller");
    assert_eq!(
        pvc["spec"]["accessModes"],
        serde_json::json!(["ReadWriteMany"])
    );
    assert!(!docs.iter().any(|d| d["kind"] == "DaemonSet"));
    assert!(
        !docs
            .iter()
            .any(|d| d["kind"] == "ValidatingAdmissionPolicy")
    );

    // Images par défaut : celles de la version de mimicctl, partout.
    let image = format!("{DEFAULT_IMAGE}:{}", version());
    let init = format!("{DEFAULT_INIT_IMAGE}:{}", version());
    assert_eq!(
        first_container_image(&docs, "mimic-controller"),
        &Value::from(image.clone())
    );
    assert_eq!(
        first_container_image(&docs, "mimic-injector"),
        &Value::from(image.clone())
    );
    let cfg = injector_config(&docs);
    assert_eq!(cfg["image"], Value::from(image));
    assert_eq!(cfg["init_image"], Value::from(init));
}

#[test]
fn cli_22_webhook_keeps_inbound_off_and_its_four_webhooks() {
    for storage in [Storage::Shared, Storage::Rwo, Storage::S3] {
        let docs = documents(&render(&InstallOptions {
            storage,
            ..InstallOptions::default()
        }));
        let cfg = injector_config(&docs);
        assert_eq!(cfg["inbound"], Value::Bool(false));
        assert!(cfg.get("cni").is_none_or(|v| v == &Value::Bool(false)));

        let hooks = find(&docs, "MutatingWebhookConfiguration", "mimic-injector")["webhooks"]
            .as_array()
            .expect("webhooks")
            .iter()
            .map(|h| h["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            hooks,
            [
                "inject.mimic.platform.io",
                "opt-in.inject.mimic.platform.io",
                "inbound.inject.mimic.platform.io",
                "inbound.opt-in.inject.mimic.platform.io",
            ]
        );
    }
}

#[test]
fn cli_22_storage_rwo_uses_a_read_write_once_claim() {
    let docs = documents(&render(&InstallOptions {
        storage: Storage::Rwo,
        ..InstallOptions::default()
    }));
    let pvc = find(&docs, "PersistentVolumeClaim", "mimic-controller");
    assert_eq!(
        pvc["spec"]["accessModes"],
        serde_json::json!(["ReadWriteOnce"])
    );
    // Le webhook suit le socle.
    assert!(
        position(&docs, "Deployment", "mimic-injector")
            > position(&docs, "Deployment", "mimic-controller")
    );
}

#[test]
fn cli_22_storage_s3_has_no_claim_and_an_s3_store() {
    let docs = documents(&render(&InstallOptions {
        storage: Storage::S3,
        ..InstallOptions::default()
    }));
    assert!(!docs.iter().any(|d| d["kind"] == "PersistentVolumeClaim"));
    let cm = find(&docs, "ConfigMap", "mimic-controller");
    let cfg: Value = serde_yaml::from_str(cm["data"]["controller.yaml"].as_str().unwrap()).unwrap();
    assert!(cfg["store"].as_str().unwrap().starts_with("s3://"));
    assert!(
        position(&docs, "Deployment", "mimic-injector")
            > position(&docs, "Deployment", "mimic-controller")
    );
}

#[test]
fn cli_22_image_option_replaces_every_default_image_and_derives_the_init_image() {
    for cni in [false, true] {
        let out = render(&InstallOptions {
            image: Some("registry.example.com/team/mimic:1.2".into()),
            cni,
            ..InstallOptions::default()
        });
        assert!(
            !out.contains(DEFAULT_IMAGE),
            "image par défaut restante (cni = {cni})"
        );
        let docs = documents(&out);
        let cfg = injector_config(&docs);
        assert_eq!(cfg["image"], "registry.example.com/team/mimic:1.2");
        assert_eq!(
            cfg["init_image"],
            "registry.example.com/team/mimic-init:1.2"
        );
        for name in ["mimic-controller", "mimic-injector"] {
            assert_eq!(
                first_container_image(&docs, name),
                "registry.example.com/team/mimic:1.2"
            );
        }
        if cni {
            let ds = find(&docs, "DaemonSet", "mimic-cni");
            assert!(
                ds.to_string()
                    .contains("registry.example.com/team/mimic:1.2")
            );
        }
    }
}

#[test]
fn cli_22_image_option_with_registry_port_derives_the_init_image() {
    let out = render(&InstallOptions {
        image: Some("localhost:5000/mimic:dev".into()),
        ..InstallOptions::default()
    });
    let cfg = injector_config(&documents(&out));
    assert_eq!(cfg["image"], "localhost:5000/mimic:dev");
    assert_eq!(cfg["init_image"], "localhost:5000/mimic-init:dev");
}

#[test]
fn cli_22_invalid_image_is_refused() {
    for bad in [
        "",
        "registry.example.com/team/mimic",     // pas de tag
        "registry.example.com/team/other:1.2", // autre nom que `mimic`
        "mimic:1.2\n  evil: true",             // injection YAML
        "registry.example.com/mimic:1 2",      // espace
    ] {
        let err = manifests(&InstallOptions {
            image: Some(bad.into()),
            ..InstallOptions::default()
        })
        .expect_err(&format!("image {bad:?} refusée"));
        assert!(
            err.contains("image"),
            "l'erreur doit parler de l'image : {err}"
        );
    }
}

#[test]
fn cli_22_cni_adds_the_daemonset_and_turns_cni_on() {
    let docs = documents(&render(&InstallOptions {
        cni: true,
        ..InstallOptions::default()
    }));
    let ds = find(&docs, "DaemonSet", "mimic-cni");
    assert_eq!(ds["metadata"]["namespace"], "mimic-system");
    assert_eq!(injector_config(&docs)["cni"], Value::Bool(true));
    // Sans l'option : pas de DaemonSet.
    let docs = documents(&render(&InstallOptions::default()));
    assert!(!docs.iter().any(|d| d["kind"] == "DaemonSet"));
}

#[test]
fn cli_22_production_guard_adds_the_admission_policy() {
    let docs = documents(&render(&InstallOptions {
        production_guard: true,
        ..InstallOptions::default()
    }));
    assert!(
        docs.iter()
            .any(|d| d["kind"] == "ValidatingAdmissionPolicy")
    );
    assert!(
        docs.iter()
            .any(|d| d["kind"] == "ValidatingAdmissionPolicyBinding")
    );
}

#[test]
fn cli_22_unknown_storage_is_refused_with_the_valid_variants() {
    let err = Storage::parse("nfs").unwrap_err();
    for v in ["shared", "rwo", "s3"] {
        assert!(err.contains(v), "{err}");
    }
    assert!(err.contains("nfs"), "{err}");
    assert_eq!(Storage::parse("shared").unwrap(), Storage::Shared);
    assert_eq!(Storage::parse("rwo").unwrap(), Storage::Rwo);
    assert_eq!(Storage::parse("s3").unwrap(), Storage::S3);
    assert_eq!(InstallOptions::default().storage, Storage::Shared);
}

#[test]
fn cli_22_output_is_deterministic() {
    let opts = InstallOptions {
        cni: true,
        production_guard: true,
        ..InstallOptions::default()
    };
    assert_eq!(render(&opts), render(&opts));
}

/// `deploy/install/` doit être le rendu de `kubectl kustomize` (scripts/render-install.sh).
/// Sans `kubectl` dans le PATH, le contrôle est sauté.
#[test]
fn cli_22_rendered_files_match_kustomize() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (dir, file) in [
        ("controller", "controller.yaml"),
        ("controller-rwo", "controller-rwo.yaml"),
        ("controller-s3", "controller-s3.yaml"),
    ] {
        let Ok(out) = std::process::Command::new("kubectl")
            .args(["kustomize", &format!("deploy/{dir}")])
            .current_dir(&root)
            .output()
        else {
            eprintln!("kubectl absent : contrôle des rendus sauté");
            return;
        };
        assert!(
            out.status.success(),
            "kubectl kustomize deploy/{dir} a échoué"
        );
        let committed = std::fs::read_to_string(root.join("deploy/install").join(file)).unwrap();
        let body = committed.split_once('\n').map_or("", |(_, rest)| rest);
        assert_eq!(
            body,
            String::from_utf8(out.stdout).unwrap(),
            "deploy/install/{file} périmé : relancer scripts/render-install.sh"
        );
    }
}
