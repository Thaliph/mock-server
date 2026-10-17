use std::path::PathBuf;

use clap::Parser;
use mimic_injector::config::InjectorConfig;

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
    match mimic_injector::server::start(config).await {
        Ok(running) => running.wait().await,
        Err(e) => {
            eprintln!("mimic-injector: {e}");
            std::process::exit(1);
        }
    }
}
