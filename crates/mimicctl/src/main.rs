use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{Api, ApiResource, DynamicObject, ListParams, Patch, PatchParams};
use mimic_controller::crd::MimicWorkload;
use mimicctl::{
    ModeOptions, Target, WorkloadData, check_namespace, export_path, load, operations_path,
    report_markdown, requests_path, sample_requests_path, status_table, workload_apply_with,
};

/// Pilote mimic depuis le poste : bascule, statut, export, diff et tir de charge (spec §9.5).
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
        /// Capture de l'entrant pendant un tir : full, metadata ou off.
        #[arg(long)]
        capture_inbound: Option<String>,
        /// Seuls ces pods basculent (`clé=valeur[,…]`, par exemple pod-template-hash=7d9f) ;
        /// les autres gardent le mode de leur configuration.
        #[arg(long)]
        selector: Option<String>,
        /// Poignée de main WebSocket sortante en mock : relay (relayée vers la dépendance)
        /// ou refuse (503, sans joindre la dépendance).
        #[arg(long)]
        mock_websocket: Option<String>,
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
    /// Tir de charge : rejoue les requêtes enregistrées contre le workload isolé, à
    /// concurrence fixe ou croissante jusqu'à saturation, et recommande CPU et mémoire
    /// (spec §5.9).
    Load(Box<LoadArgs>),
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

/// Options de `mimicctl load`.
#[derive(clap::Args)]
struct LoadArgs {
    /// Workload : requêtes lues sur le contrôleur, isolement vérifié, CPU et mémoire de
    /// ses pods relevés. Facultatif avec --from.
    workload: Option<String>,
    #[arg(short, long)]
    namespace: Option<String>,
    /// URL du service visé : http://hôte[:port][/préfixe].
    #[arg(long)]
    target: String,
    /// Fichier JSONL d'échanges (`mimicctl requests --format jsonl`) au lieu du contrôleur.
    #[arg(long)]
    from: Option<PathBuf>,
    /// Requêtes lues sur le contrôleur.
    #[arg(long, default_value_t = 1000)]
    limit: usize,
    /// Header ajouté ou remplacé (`nom: valeur`), répétable.
    #[arg(long = "header", short = 'H')]
    headers: Vec<String>,
    /// HTTP/2 sans TLS (gRPC).
    #[arg(long)]
    http2: bool,
    /// Concurrence du palier fixe.
    #[arg(long, default_value_t = 8, conflicts_with = "ramp")]
    concurrency: u32,
    /// Nombre exact de requêtes du palier fixe.
    #[arg(long, conflicts_with_all = ["duration", "ramp"])]
    requests: Option<u64>,
    /// Durée du palier fixe.
    #[arg(long, default_value = "60s", conflicts_with = "ramp")]
    duration: String,
    /// Montée par paliers jusqu'à saturation.
    #[arg(long)]
    ramp: bool,
    /// Concurrence du premier palier de la montée.
    #[arg(long, default_value_t = 1, requires = "ramp")]
    start: u32,
    /// Facteur de concurrence d'un palier au suivant (> 1).
    #[arg(long, default_value_t = 2.0, requires = "ramp")]
    factor: f64,
    /// Concurrence du dernier palier.
    #[arg(long, default_value_t = 256, requires = "ramp")]
    max: u32,
    /// Durée de chaque palier de la montée.
    #[arg(long, default_value = "60s", requires = "ramp")]
    step_duration: String,
    /// Taux d'erreur au-delà duquel un palier est saturé (0.01 ou 1%).
    #[arg(long, default_value = "1%")]
    max_error_rate: String,
    /// p99 au-delà duquel un palier est saturé (500ms, 2s).
    #[arg(long)]
    max_p99: Option<String>,
    /// Hausse de débit minimale, en part de la hausse de concurrence (10% : +10 % de
    /// débit pour une concurrence doublée).
    #[arg(long, default_value = "10%")]
    plateau: String,
    /// Délai d'une requête, réponse complète comprise.
    #[arg(long, default_value = "10s")]
    timeout: String,
    /// Intervalle des relevés de metrics.k8s.io.
    #[arg(long, default_value = "15s")]
    sample_interval: String,
    /// Marge ajoutée au CPU moyen mesuré pour `requests.cpu`.
    #[arg(long, default_value = "30%")]
    cpu_headroom: String,
    /// Marge ajoutée à la mémoire maximale mesurée pour `requests.memory`.
    #[arg(long, default_value = "20%")]
    memory_headroom: String,
    /// Pods dont relever CPU et mémoire (`clé=valeur[,…]`), au lieu de ceux du contrôleur.
    #[arg(long)]
    pod_selector: Option<String>,
    /// Format du rapport sur la sortie standard.
    #[arg(long, default_value = "text", value_parser = ["text", "markdown", "json"])]
    format: String,
    /// Tire même si le workload n'est pas en mock, pods alignés.
    #[arg(long)]
    allow_live_dependencies: bool,
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
            capture_inbound,
            selector,
            mock_websocket,
            namespace: ns,
        } => {
            let opts = ModeOptions {
                snapshot,
                capture_inbound,
                selector,
                mock_websocket,
            };
            // Validation avant toute connexion au cluster (CLI-02, CLI-07, CLI-08).
            workload_apply_with(ns.as_deref().unwrap_or("default"), &workload, &mode, &opts)?;
            let client = client().await?;
            let ns = namespace(ns, &client).await;
            let mw = workload_apply_with(&ns, &workload, &mode, &opts)?;
            check_namespace(&ns, &mode, namespace_labels(&client, &ns).await.as_ref())?;
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
        Command::Load(args) => {
            let LoadArgs {
                workload,
                namespace: ns,
                target,
                from,
                limit,
                headers,
                http2,
                concurrency,
                requests,
                duration,
                ramp,
                start,
                factor,
                max,
                step_duration,
                max_error_rate,
                max_p99,
                plateau,
                timeout,
                sample_interval,
                cpu_headroom,
                memory_headroom,
                pod_selector,
                format,
                allow_live_dependencies,
            } = *args;
            // Validation avant toute connexion au cluster.
            let target = load::Target::parse(&target, http2)?;
            let extra = headers
                .iter()
                .map(|h| mimic_core::load::parse_header(h))
                .collect::<Result<Vec<_>, _>>()?;
            let plan = if ramp {
                load::Plan::Ramp {
                    steps: mimic_core::load::ramp(start, factor, max)?,
                    step_duration: load::parse_duration(&step_duration)?,
                }
            } else {
                if concurrency == 0 {
                    return Err("--concurrency : au moins 1".into());
                }
                let stop = match requests {
                    Some(0) => return Err("--requests : au moins 1".into()),
                    Some(n) => load::Stop::Requests(n),
                    None => load::Stop::Duration(load::parse_duration(&duration)?),
                };
                load::Plan::Fixed { concurrency, stop }
            };
            let criteria = mimic_core::load::Criteria {
                max_error_rate: load::parse_ratio(&max_error_rate)?,
                max_p99_us: max_p99
                    .as_deref()
                    .map(load::parse_duration)
                    .transpose()?
                    .map(|d| d.as_micros() as u64),
                plateau: load::parse_ratio(&plateau)?,
            };
            let (cpu_headroom, memory_headroom) = (
                load::parse_ratio(&cpu_headroom)?,
                load::parse_ratio(&memory_headroom)?,
            );
            let pod_selector = pod_selector
                .as_deref()
                .map(|s| mimicctl::parse_selector(s).map(|_| s.to_string()))
                .transpose()?;
            if workload.is_none() && from.is_none() {
                return Err("un workload ou --from requis".into());
            }
            let mut notes = vec![];
            let mut exchanges = vec![];
            if let Some(path) = &from {
                exchanges = read_exchanges(path)?;
            }
            let mut sampler = None;
            if let Some(name) = &workload {
                let client = client().await?;
                let ns = namespace(ns, &client).await;
                let known = controller_json(client.clone(), "/v1/workloads").await?;
                let w = known
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|w| w["namespace"] == ns.as_str() && w["name"] == name.as_str())
                    .cloned()
                    .ok_or_else(|| format!("{ns}/{name} inconnu du contrôleur"))?;
                match load::isolation(&w) {
                    Ok(()) => {}
                    Err(e) if allow_live_dependencies => {
                        notes.push(format!(
                            "isolement non vérifié (--allow-live-dependencies) : {e}"
                        ));
                    }
                    Err(e) => {
                        eprintln!("mimicctl: {ns}/{name} : {e}");
                        return Ok(2);
                    }
                }
                if from.is_none() {
                    let path = requests_path(&ns, name, "jsonl", limit);
                    let body = controller_get(client.clone(), &path).await?;
                    exchanges = parse_exchanges(&String::from_utf8_lossy(&body))?;
                }
                let pods = load::selected_pods(&w);
                sampler = Some(metrics_sampler(client, ns, pod_selector, pods));
            } else {
                notes.push(
                    "sans workload : isolement des dépendances non vérifié, CPU et mémoire non relevés"
                        .into(),
                );
            }
            let corpus = mimic_core::load::Corpus::new(&exchanges, &extra);
            if corpus.is_empty() {
                return Err("aucune requête entrante enregistrée à rejouer".into());
            }
            let opts = load::Options {
                timeout: load::parse_duration(&timeout)?,
                criteria: criteria.clone(),
                sample_interval: load::parse_duration(&sample_interval)?,
                sampler,
                progress: true,
            };
            let target_text = format!(
                "http://{}{}{}",
                target.authority,
                target.prefix,
                if http2 { " (HTTP/2)" } else { "" }
            );
            let (len, discarded) = (corpus.len(), corpus.truncated());
            let out = load::run(std::sync::Arc::new(corpus), &target, &plan, &opts).await;
            notes.extend(out.notes);
            let report = mimic_core::load::Report::new(
                &target_text,
                len,
                discarded,
                out.steps,
                &criteria,
                cpu_headroom,
                memory_headroom,
                notes,
            );
            match format.as_str() {
                "json" => println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                ),
                "markdown" => print!("{}", mimic_core::load::render_markdown(&report)),
                _ => print!("{}", mimic_core::load::render_text(&report)),
            }
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

/// Labels du namespace, pour le garde-fou production ; `None` si on ne peut pas les lire
/// (droit `get` absent) : le contrôleur applique alors le garde-fou seul.
async fn namespace_labels(
    client: &kube::Client,
    ns: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let api: Api<Namespace> = Api::all(client.clone());
    match api.get_opt(ns).await {
        Ok(Some(n)) => Some(n.metadata.labels.unwrap_or_default()),
        Ok(None) => None,
        Err(e) => {
            eprintln!(
                "mimicctl: labels du namespace {ns} illisibles ({e}) ; le contrôleur appliquera le garde-fou production"
            );
            None
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

/// Échanges d'un fichier JSONL (`mimicctl requests --format jsonl`).
fn read_exchanges(path: &Path) -> Result<Vec<mimic_core::model::Exchange>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("lecture de {}: {e}", path.display()))?;
    parse_exchanges(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_exchanges(text: &str) -> Result<Vec<mimic_core::model::Exchange>, String> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("ligne {}: {e}", i + 1)))
        .collect()
}

/// Relevés `metrics.k8s.io` des pods du workload : ceux de `selector`, sinon ceux que le
/// contrôleur connaît.
fn metrics_sampler(
    client: kube::Client,
    ns: String,
    selector: Option<String>,
    pods: Vec<String>,
) -> load::Sampler {
    let resource = ApiResource {
        group: "metrics.k8s.io".into(),
        version: "v1beta1".into(),
        api_version: "metrics.k8s.io/v1beta1".into(),
        kind: "PodMetrics".into(),
        plural: "pods".into(),
    };
    std::sync::Arc::new(move || {
        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &ns, &resource);
        let (selector, pods) = (selector.clone(), pods.clone());
        Box::pin(async move {
            let params = match &selector {
                Some(s) => ListParams::default().labels(s),
                None => ListParams::default(),
            };
            let list = api
                .list(&params)
                .await
                .map_err(|e| format!("metrics.k8s.io: {e}"))?;
            Ok(list
                .items
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .filter_map(|m| load::pod_usage(&m))
                .filter(|(pod, _)| selector.is_some() || pods.contains(pod))
                .collect())
        })
    })
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
    let name = mimicctl::controller_pod(&list.items)
        .map_err(|e| format!("{e} dans {CONTROLLER_NAMESPACE}"))?;
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
