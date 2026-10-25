//! Critère CNI-02 de la spec (§3.1b) : installation du plugin dans la configuration CNI.

use std::path::Path;

use mimic_cni::install::{Dirs, install, uninstall};
use serde_json::{Value, json};

fn dirs(root: &Path) -> Dirs {
    let conf = root.join("net.d");
    let sa = root.join("sa");
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::create_dir_all(&sa).unwrap();
    std::fs::write(sa.join("token"), "jeton-1").unwrap();
    std::fs::write(sa.join("ca.crt"), "-----BEGIN CERTIFICATE-----\nCA\n").unwrap();
    Dirs {
        conf_dir: conf,
        host_conf_dir: "/etc/cni/net.d".into(),
        service_account_dir: sa,
        server: "https://10.96.0.1:443".into(),
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn ours(plugins: &Value) -> usize {
    plugins
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["type"] == "mimic-cni")
        .count()
}

#[test]
fn cni_02_plugin_is_chained_last_once_and_removed_on_stop() {
    let root = tempfile::tempdir().unwrap();
    let d = dirs(root.path());
    let calico = json!({
        "cniVersion": "1.0.0", "name": "k8s-pod-network",
        "plugins": [{"type": "calico"}, {"type": "portmap"}]
    });
    std::fs::write(d.conf_dir.join("10-calico.conflist"), calico.to_string()).unwrap();
    std::fs::write(d.conf_dir.join("99-autre.conflist"), calico.to_string()).unwrap();

    install(&d).unwrap();
    let conf = read(&d.conf_dir.join("10-calico.conflist"));
    let plugins = conf["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 3);
    assert_eq!(plugins[2]["type"], "mimic-cni");
    assert_eq!(
        plugins[2]["kubeconfig"],
        "/etc/cni/net.d/mimic-cni.d/kubeconfig"
    );
    assert_eq!(
        read(&d.conf_dir.join("99-autre.conflist")),
        calico,
        "seul le premier"
    );

    // Idempotent.
    install(&d).unwrap();
    assert_eq!(
        ours(&read(&d.conf_dir.join("10-calico.conflist"))["plugins"]),
        1
    );

    // Kubeconfig : serveur, CA et jeton recopiés, chemins de l'hôte.
    let kube = std::fs::read_to_string(d.conf_dir.join("mimic-cni.d/kubeconfig")).unwrap();
    assert!(kube.contains("server: https://10.96.0.1:443"), "{kube}");
    assert!(kube.contains("/etc/cni/net.d/mimic-cni.d/ca.crt"), "{kube}");
    assert!(
        kube.contains("tokenFile: /etc/cni/net.d/mimic-cni.d/token"),
        "{kube}"
    );
    assert_eq!(
        std::fs::read_to_string(d.conf_dir.join("mimic-cni.d/token")).unwrap(),
        "jeton-1"
    );
    // Jeton renouvelé : recopié à l'installation suivante.
    std::fs::write(d.service_account_dir.join("token"), "jeton-2").unwrap();
    install(&d).unwrap();
    // Le jeton donne `get` sur tous les pods : lisible par root seul, sur l'hôte.
    let mode = |f: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(d.conf_dir.join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("mimic-cni.d"), 0o700);
    assert_eq!(mode("mimic-cni.d/token"), 0o600);
    assert_eq!(mode("mimic-cni.d/kubeconfig"), 0o600);
    assert_eq!(
        std::fs::read_to_string(d.conf_dir.join("mimic-cni.d/token")).unwrap(),
        "jeton-2"
    );

    // Le fournisseur réseau réécrit son fichier : réajouté.
    std::fs::write(d.conf_dir.join("10-calico.conflist"), calico.to_string()).unwrap();
    install(&d).unwrap();
    assert_eq!(
        ours(&read(&d.conf_dir.join("10-calico.conflist"))["plugins"]),
        1
    );

    // Arrêt : retiré.
    uninstall(&d).unwrap();
    assert_eq!(read(&d.conf_dir.join("10-calico.conflist")), calico);
}

#[test]
fn cni_02_single_plugin_conf_becomes_a_conflist() {
    let root = tempfile::tempdir().unwrap();
    let d = dirs(root.path());
    let bridge = json!({"cniVersion": "0.4.0", "name": "kindnet", "type": "ptp", "ipam": {"type": "host-local"}});
    std::fs::write(d.conf_dir.join("10-kindnet.conf"), bridge.to_string()).unwrap();
    install(&d).unwrap();
    assert!(!d.conf_dir.join("10-kindnet.conf").exists());
    let conf = read(&d.conf_dir.join("10-kindnet.conflist"));
    assert_eq!(conf["cniVersion"], "0.4.0");
    assert_eq!(conf["name"], "kindnet");
    let plugins = conf["plugins"].as_array().unwrap();
    assert_eq!(plugins[0]["type"], "ptp");
    assert_eq!(plugins[0]["ipam"]["type"], "host-local");
    assert_eq!(plugins[1]["type"], "mimic-cni");
}

#[test]
fn cni_02_nothing_to_chain_onto_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let d = dirs(root.path());
    let err = install(&d).unwrap_err();
    assert!(err.contains("aucune configuration CNI"), "{err}");
}

// ---- CNI-03 : l'installateur n'écrit jamais à travers un lien symbolique ----

fn symlink_to_witness(root: &Path, link: &Path) -> std::path::PathBuf {
    let witness = root.join("temoin");
    std::fs::write(&witness, "temoin-intact").unwrap();
    std::os::unix::fs::symlink(&witness, link).unwrap();
    witness
}

fn assert_refused(root: &Path, link: &Path, witness: &Path, err: &str) {
    assert!(
        err.contains(&link.display().to_string()),
        "l'erreur doit nommer le chemin {} : {err}",
        link.display()
    );
    assert_eq!(
        std::fs::read_to_string(witness).unwrap(),
        "temoin-intact",
        "rien n'est écrit à travers le lien"
    );
    assert!(
        std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "le lien reste un lien"
    );
    let _ = root;
}

#[test]
fn cni_03_install_refuses_symlinked_conflist() {
    let root = tempfile::tempdir().unwrap();
    let d = dirs(root.path());
    let calico = json!({"cniVersion": "1.0.0", "name": "n", "plugins": [{"type": "calico"}]});
    let link = d.conf_dir.join("10-calico.conflist");
    let witness = symlink_to_witness(root.path(), &link);
    std::fs::write(&witness, calico.to_string()).unwrap();
    let before = std::fs::read_to_string(&witness).unwrap();

    let err = install(&d).expect_err("une .conflist en lien symbolique est refusée");
    assert!(err.contains(&link.display().to_string()), "{err}");
    assert_eq!(std::fs::read_to_string(&witness).unwrap(), before);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn cni_03_install_refuses_symlinked_own_dir_file() {
    for name in ["token", "ca.crt", "kubeconfig"] {
        let root = tempfile::tempdir().unwrap();
        let d = dirs(root.path());
        let calico = json!({"cniVersion": "1.0.0", "name": "n", "plugins": [{"type": "calico"}]});
        std::fs::write(d.conf_dir.join("10-calico.conflist"), calico.to_string()).unwrap();
        let own = d.conf_dir.join("mimic-cni.d");
        std::fs::create_dir_all(&own).unwrap();
        let link = own.join(name);
        let witness = symlink_to_witness(root.path(), &link);

        let err = install(&d).expect_err("un fichier de mimic-cni.d/ en lien est refusé");
        assert_refused(root.path(), &link, &witness, &err);
    }
}

#[test]
fn cni_03_install_refuses_symlinked_binary_target() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    let d = dirs(root.path());
    let calico = json!({"cniVersion": "1.0.0", "name": "n", "plugins": [{"type": "calico"}]});
    std::fs::write(d.conf_dir.join("10-calico.conflist"), calico.to_string()).unwrap();
    let bin_dir = root.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let link = bin_dir.join("mimic-cni");
    let witness = symlink_to_witness(root.path(), &link);

    // L'installateur est le binaire lui-même (`mimic-cni install`) : il doit échouer avant
    // d'entrer dans sa boucle de rafraîchissement.
    let mut child = Command::new(env!("CARGO_BIN_EXE_mimic-cni"))
        .arg("install")
        .args(["--bin-dir"])
        .arg(&bin_dir)
        .args(["--conf-dir"])
        .arg(&d.conf_dir)
        .args(["--service-account-dir"])
        .arg(&d.service_account_dir)
        .env("KUBERNETES_SERVICE_HOST", "10.96.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Garde-fou : tant que le comportement manque, l'installateur boucle ; on le tue.
    let pid = child.id();
    let (done, rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if rx.recv_timeout(std::time::Duration::from_secs(20)).is_err() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    });
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();
    let _ = done.send(());
    watchdog.join().unwrap();

    assert!(!status.success(), "refus du lien : code non nul ({stderr})");
    assert_refused(root.path(), &link, &witness, &stderr);
}
