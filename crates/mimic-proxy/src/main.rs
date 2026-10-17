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
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MIMIC_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let config = match Config::load(args.config.as_deref()) {
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
