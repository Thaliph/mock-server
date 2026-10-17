use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use mimic_controller::crd::MimicWorkload;
use mimicctl::{
    Target, WorkloadData, export_path, operations_path, report_markdown, requests_path,
    sample_requests_path, status_table, workload_apply,
};

/// Pilote mimic depuis le poste : bascule, statut, export et diff (spec §9.5).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bascule tous les pods d'un workload (crée ou met à jour sa MimicWorkload).
    Mode {
        workload: String,
        /// passthrough, record, mock ou mock-partial.
        mode: String,
        /// Rejoue ce snapshot au lieu d'en construire un.
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(short, long)]
        namespace: Option<String>,
    },
    /// Mode souhaité, snapshot servi et pods alignés.
    Status {
        workload: Option<String>,
        #[arg(short, long)]
        namespace: Option<String>,
        #[arg(short = 'A', long)]
        all_namespaces: bool,
    },
    /// Document OpenAPI découvert, lu sur l'interface du contrôleur.
    Export {
        workload: String,
        #[arg(short, long)]
        namespace: Option<String>,
        /// API du service.
        #[arg(long, conflicts_with = "outbound")]
        inbound: bool,
        /// API d'une dépendance, par authority (`api.stripe.com`).
        #[arg(long)]
        outbound: Option<String>,
        #[arg(long, default_value = "3.1", value_parser = ["3.1", "3.0"])]
        openapi_version: String,
        /// Fichier de sortie (`.yaml`/`.yml` : YAML, sinon JSON) ; absent, la sortie standard.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Requêtes entrantes enregistrées, pour le rejeu (spec §5.7).
    Requests {
        workload: String,
        #[arg(short, long)]
        namespace: Option<String>,
        #[arg(long, default_value = "json", value_parser = ["json", "jsonl", "k6"])]
        format: String,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Rapport Markdown de la découverte : API, opérations les plus appelées et headers.
    Report {
        workload: Option<String>,
        #[arg(short, long)]
        namespace: Option<String>,
        #[arg(short = 'A', long)]
        all_namespaces: bool,
        /// Nombre d'opérations les plus appelées par workload.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Différence entre deux documents OpenAPI (YAML ou JSON).
    Diff {
        old: PathBuf,
        new: PathBuf,
        #[arg(long)]
        json: bool,
        /// Code de sortie 1 s'il y a un changement cassant.
        #[arg(long)]
        fail_on_breaking: bool,
    },
}

const CONTROLLER_NAMESPACE: &str = "mimic-system";
const CONTROLLER_SELECTOR: &str = "app.kubernetes.io/name=mimic-controller";
const UI_PORT: u16 = 8081;
/// Requêtes lues par sens pour trouver un exemple de headers à chaque opération.
const REPORT_SAMPLES: usize = 500;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    mimic_controller::install_crypto_provider();
    match run(cli.command).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("mimicctl: {e}");
            std::process::exit(2);
        }
    }
}

async fn client() -> Result<kube::Client, String> {
    kube::Client::try_default()
        .await
        .map_err(|e| format!("kubeconfig: {e}"))
}

async fn namespace(ns: Option<String>, client: &kube::Client) -> String {
    ns.unwrap_or_else(|| client.default_namespace().to_string())
}

async fn run(command: Command) -> Result<i32, String> {
    match command {
        Command::Mode {
            workload,
            mode,
            snapshot,
            namespace: ns,
        } => {
            // Validation avant toute connexion au cluster (CLI-02).
            workload_apply(ns.as_deref().unwrap_or("default"), &workload, &mode, None)?;
            let client = client().await?;
            let ns = namespace(ns, &client).await;
            let mw = workload_apply(&ns, &workload, &mode, snapshot.as_deref())?;
            let api: Api<MimicWorkload> = Api::namespaced(client, &ns);
            api.patch(
                &workload,
                &PatchParams::apply("mimicctl"),
                &Patch::Apply(&mw),
            )
            .await
            .map_err(|e| format!("mise à jour de {ns}/{workload}: {e}"))?;
            println!("{ns}/{workload} : mode souhaité {mode}");
            Ok(0)
        }
        Command::Status {
            workload,
            namespace: ns,
            all_namespaces,
        } => {
            let client = client().await?;
            let api: Api<MimicWorkload> = if all_namespaces {
                Api::all(client)
            } else {
                let ns = namespace(ns, &client).await;
                Api::namespaced(client, &ns)
            };
            let mut items = api
                .list(&ListParams::default())
                .await
                .map_err(|e| format!("liste des MimicWorkload: {e}"))?
                .items;
            if let Some(w) = workload {
                items.retain(|mw| mw.metadata.name.as_deref() == Some(w.as_str()));
            }
            print!("{}", status_table(&items));
            Ok(0)
        }
        Command::Export {
            workload,
            namespace: ns,
            inbound,
            outbound,
            openapi_version,
            output,
        } => {
            let target = match (inbound, outbound) {
                (_, Some(a)) => Target::Outbound(a),
                (true, None) => Target::Inbound,
                (false, None) => return Err("--inbound ou --outbound <authority> requis".into()),
            };
            let client = client().await?;
            let ns = namespace(ns, &client).await;
            let body = controller_get(client, &export_path(&ns, &workload, &target)).await?;
            let mut doc: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|e| format!("réponse du contrôleur illisible: {e}"))?;
            if openapi_version == "3.0" {
                doc = mimic_core::openapi::to_3_0(&doc);
            }
            write_doc(&doc, output.as_deref())?;
            Ok(0)
        }
        Command::Requests {
            workload,
            namespace: ns,
            format,
            limit,
        } => {
            let client = client().await?;
            let ns = namespace(ns, &client).await;
            let body =
                controller_get(client, &requests_path(&ns, &workload, &format, limit)).await?;
            print!("{}", String::from_utf8_lossy(&body));
            Ok(0)
        }
        Command::Report {
            workload,
            namespace: ns,
            all_namespaces,
            top,
        } => {
            let client = client().await?;
            let ns = if all_namespaces {
                None
            } else {
                Some(namespace(ns, &client).await)
            };
            let known = controller_json(client.clone(), "/v1/workloads").await?;
            let mut workloads = vec![];
            for w in known.as_array().into_iter().flatten() {
                let (wns, name) = (w["namespace"].as_str(), w["name"].as_str());
                let (Some(wns), Some(name)) = (wns, name) else {
                    continue;
                };
                if ns.as_deref().is_some_and(|n| n != wns)
                    || workload.as_deref().is_some_and(|n| n != name)
                {
                    continue;
                }
                let operations =
                    controller_json(client.clone(), &operations_path(wns, name)).await?;
                let mut requests = vec![];
                for direction in ["inbound", "outbound"] {
                    let path = sample_requests_path(wns, name, direction, REPORT_SAMPLES);
                    if let serde_json::Value::Array(list) =
                        controller_json(client.clone(), &path).await?
                    {
                        requests.extend(list);
                    }
                }
                let pods = w["pods"].as_array().map_or(0, Vec::len);
                workloads.push(WorkloadData {
                    namespace: wns.into(),
                    name: name.into(),
                    mode: w["desired"]["mode"].as_str().map(String::from),
                    pods,
                    aligned: w["aligned"].as_u64().unwrap_or(0) as usize,
                    operations: operations.as_array().cloned().unwrap_or_default(),
                    requests,
                });
            }
            print!("{}", report_markdown(&workloads, top));
            Ok(0)
        }
        Command::Diff {
            old,
            new,
            json,
            fail_on_breaking,
        } => {
            let d = mimic_core::diff::diff(&read_doc(&old)?, &read_doc(&new)?);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(|e| e.to_string())?
                );
            } else if d.changes.is_empty() {
                println!("aucune différence");
            } else {
                print!("{}", d.to_text());
            }
            Ok(i32::from(fail_on_breaking && d.is_breaking()))
        }
    }
}

fn read_doc(path: &Path) -> Result<serde_json::Value, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("lecture de {}: {e}", path.display()))?;
    // YAML est un sur-ensemble de JSON : un seul parseur pour les deux.
    serde_yaml::from_str(&text).map_err(|e| format!("{} illisible: {e}", path.display()))
}

fn write_doc(doc: &serde_json::Value, output: Option<&Path>) -> Result<(), String> {
    let yaml = output
        .and_then(|p| p.extension())
        .is_some_and(|e| e == "yaml" || e == "yml");
    let text = if yaml {
        serde_yaml::to_string(doc).map_err(|e| e.to_string())?
    } else {
        format!(
            "{}\n",
            serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?
        )
    };
    match output {
        Some(p) => std::fs::write(p, text).map_err(|e| format!("écriture de {}: {e}", p.display())),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

async fn controller_json(client: kube::Client, path: &str) -> Result<serde_json::Value, String> {
    let body = controller_get(client, path).await?;
    serde_json::from_slice(&body)
        .map_err(|e| format!("réponse du contrôleur illisible ({path}): {e}"))
}

/// `GET` sur l'interface du contrôleur, liée à 127.0.0.1 dans son pod : on y passe par
/// un port-forward, comme `kubectl port-forward` (même RBAC).
async fn controller_get(client: kube::Client, path: &str) -> Result<Bytes, String> {
    let pods: Api<Pod> = Api::namespaced(client, CONTROLLER_NAMESPACE);
    let list = pods
        .list(&ListParams::default().labels(CONTROLLER_SELECTOR))
        .await
        .map_err(|e| format!("pods du contrôleur: {e}"))?;
    let running = list
        .items
        .iter()
        .find(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"));
    let name = running
        .and_then(|p| p.metadata.name.clone())
        .ok_or_else(|| format!("aucun pod du contrôleur en marche dans {CONTROLLER_NAMESPACE}"))?;
    let mut pf = pods
        .portforward(&name, &[UI_PORT])
        .await
        .map_err(|e| format!("port-forward vers {name}: {e}"))?;
    let stream = pf.take_stream(UI_PORT).ok_or("port-forward sans flux")?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("connexion au contrôleur: {e}"))?;
    let conn = tokio::spawn(conn);
    let req = hyper::Request::get(path)
        .header("host", "127.0.0.1")
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("requête au contrôleur: {e}"))?;
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("réponse du contrôleur: {e}"))?
        .to_bytes();
    drop(sender);
    conn.abort();
    if !status.is_success() {
        return Err(format!(
            "contrôleur : {status} {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    Ok(body)
}
