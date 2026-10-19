use std::path::PathBuf;

use clap::Parser;
use mimic_injector::config::InjectorConfig;
use tokio::signal::unix::{SignalKind, signal};

/// Webhook d'admission qui injecte mimic dans les pods (spec §9.4).
#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(long, short, env = "MIMIC_INJECTOR_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let config = match InjectorConfig::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mimic-injector: {e}");
            std::process::exit(1);
        }
    };
    let running = match mimic_injector::server::start(config).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mimic-injector: {e}");
            std::process::exit(1);
        }
    };
    // PID 1 sans init dans l'image : sans ce handler, SIGTERM serait ignoré et chaque arrêt
    // attendrait le SIGKILL de fin de grâce.
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mimic-injector: signal SIGTERM: {e}");
            std::process::exit(1);
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("arrêt : admissions en cours terminées, puis sortie");
    running.stop().await;
}
