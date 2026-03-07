mod bridge;
mod config;
mod mumble;
mod server;
mod session;
mod webrtc;
mod ws;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("mumdota=info")),
        )
        .init();

    // Load configuration
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let config = config::Config::load(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to load config from '{}': {}", config_path, e))?;

    tracing::info!(
        "Mumble server: {}:{}",
        config.mumble.host,
        config.mumble.port
    );

    // Start the server
    server::run(config).await
}
