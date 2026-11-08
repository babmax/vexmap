use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use vexmap_sidecar::{Config, run};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let result = match Config::from_env(|name| std::env::var(name).ok()) {
        Ok(config) => run(config).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}
