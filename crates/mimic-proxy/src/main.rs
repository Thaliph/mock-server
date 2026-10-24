use std::path::PathBuf;

use clap::Parser;
use mimic_proxy::config::Config;

/// Sidecar mimic : découverte d'API et mock des dépendances.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Fichier de configuration YAML.
    #[arg(long, short, env = "MIMIC_CONFIG")]
    config: Option<PathBuf>,
    /// Interroge l'API admin du sidecar (`/healthz`…) et sort : sonde `exec` (spec §3.3.4).
    #[arg(long, value_name = "CHEMIN")]
    probe: Option<String>,
    /// Mock local (spec §9.7) : sert ce snapshot JSONL (`mimicctl pull`), sans contrôleur.
    #[arg(long, value_name = "FICHIER")]
    mock_from: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    if let Some(path) = &args.probe {
        // Sans runtime ni logs : la sonde tourne toutes les quelques secondes.
        let outcome = Config::load(args.config.as_deref())
            .and_then(|config| mimic_proxy::probe(&config, path));
        if let Err(e) = outcome {
            eprintln!("mimic: sonde {path} : {e}");
            std::process::exit(1);
        }
        return;
    }
    run(args);
}

#[tokio::main]
async fn run(args: Args) {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MIMIC_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    // `--mock-from` vaut `MIMIC_MOCK_FROM`, avant la validation (sel facultatif en local).
    let mock_from = args.mock_from.as_ref().map(|p| p.display().to_string());
    let config = match Config::load_with(args.config.as_deref(), |k| match (k, &mock_from) {
        ("MIMIC_MOCK_FROM", Some(file)) => Some(file.clone()),
        _ => std::env::var(k).ok(),
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mimic: {e}");
            std::process::exit(2);
        }
    };
    let running = match mimic_proxy::start(config).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mimic: {e}");
            std::process::exit(1);
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("arrêt demandé");
    running.shutdown().await;
}
