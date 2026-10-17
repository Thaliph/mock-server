use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mimic-controller", about = "Plan de contrôle de mimic")]
struct Args {
    /// Fichier de configuration YAML.
    #[arg(long, env = "MIMIC_CONTROLLER_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Affiche le manifeste de la CRD MimicWorkload.
    Crd,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Some(Command::Crd) = args.command {
        print!("{}", mimic_controller::crd::crd_yaml());
        return;
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MIMIC_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let config = match mimic_controller::config::ControllerConfig::load(args.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let running = match mimic_controller::start(config).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal SIGTERM");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    running.shutdown().await;
}
