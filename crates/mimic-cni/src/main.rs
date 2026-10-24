//! `mimic-cni` : plugin CNI quand le runtime l'appelle (`CNI_COMMAND`), installateur sinon
//! (`mimic-cni install`, conteneur du DaemonSet).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use mimic_cni::install::{Dirs, install, uninstall};
use mimic_cni::{Host, Invocation, run};
use mimic_init::apply::{Failure, Settings, apply};

/// Rafraîchissement du jeton et réinsertion du plugin.
const REFRESH: Duration = Duration::from_secs(30);

struct Node;

impl Host for Node {
    fn pod_annotations(
        &self,
        kubeconfig: &str,
        namespace: &str,
        name: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            let config = kube::config::Kubeconfig::read_from(kubeconfig)
                .map_err(|e| format!("kubeconfig {kubeconfig}: {e}"))?;
            let config = kube::Config::from_custom_kubeconfig(
                config,
                &kube::config::KubeConfigOptions::default(),
            )
            .await
            .map_err(|e| format!("kubeconfig {kubeconfig}: {e}"))?;
            let client = kube::Client::try_from(config).map_err(|e| e.to_string())?;
            let pod = kube::Api::<Pod>::namespaced(client, namespace)
                .get(name)
                .await
                .map_err(|e| format!("pod {namespace}/{name}: {e}"))?;
            Ok(pod.metadata.annotations.unwrap_or_default())
        })
    }

    fn apply(&self, netns: &str, settings: &Settings) -> Result<(), String> {
        // Le fil courant entre dans le namespace réseau du pod ; les commandes iptables qu'il
        // lance en héritent.
        let ns = std::fs::File::open(netns).map_err(|e| format!("{netns}: {e}"))?;
        nix::sched::setns(ns, nix::sched::CloneFlags::CLONE_NEWNET)
            .map_err(|e| format!("setns {netns}: {e}"))?;
        // La sortie standard est réservée au résultat CNI : le déroulé va au journal.
        apply(settings, &mut std::io::stderr()).map_err(|e| match e {
            Failure::Config(e) | Failure::Apply(e) => e,
        })
    }
}

fn plugin(command: String) -> ! {
    let mut stdin = vec![];
    let _ = std::io::stdin().read_to_end(&mut stdin);
    let out = run(
        &Invocation {
            command,
            netns: std::env::var("CNI_NETNS").ok(),
            args: std::env::var("CNI_ARGS").unwrap_or_default(),
            stdin,
        },
        &Node,
    );
    print!("{}", out.stdout);
    std::process::exit(out.code);
}

/// Argument `--nom valeur`, sinon la valeur par défaut.
fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn installer(args: &[String]) -> Result<(), String> {
    let bin_dir = PathBuf::from(arg(args, "--bin-dir", "/host/opt/cni/bin"));
    let host =
        std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| "KUBERNETES_SERVICE_HOST absent")?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
    let server = if host.contains(':') {
        format!("https://[{host}]:{port}")
    } else {
        format!("https://{host}:{port}")
    };
    let dirs = Dirs {
        conf_dir: PathBuf::from(arg(args, "--conf-dir", "/host/etc/cni/net.d")),
        host_conf_dir: arg(args, "--host-conf-dir", "/etc/cni/net.d"),
        service_account_dir: PathBuf::from(arg(
            args,
            "--service-account-dir",
            "/var/run/secrets/kubernetes.io/serviceaccount",
        )),
        server,
    };
    // Binaire copié à côté puis renommé : le runtime n'exécute jamais une copie partielle.
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = bin_dir.join(".mimic-cni.tmp");
    std::fs::copy(&me, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, bin_dir.join("mimic-cni")).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| e.to_string())?;
        loop {
            match install(&dirs) {
                Ok(()) => {}
                Err(e) => eprintln!("mimic-cni: {e}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(REFRESH) => {}
                _ = term.recv() => break,
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        uninstall(&dirs)?;
        eprintln!("mimic-cni: plugin retiré de la configuration CNI");
        Ok(())
    })
}

fn main() {
    if let Ok(command) = std::env::var("CNI_COMMAND") {
        plugin(command);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => {
            if let Err(e) = installer(&args[1..]) {
                eprintln!("mimic-cni: {e}");
                std::process::exit(1);
            }
        }
        Some("--version") => println!("mimic-cni {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!(
                "usage : mimic-cni install [--bin-dir D] [--conf-dir D] [--host-conf-dir D] ; \
                 en plugin, appelé par le runtime avec CNI_COMMAND"
            );
            std::process::exit(2);
        }
    }
}
