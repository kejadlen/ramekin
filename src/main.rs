use color_eyre::eyre::{Result, WrapErr};
use tracing::info;

mod agent;
mod bridge;
mod tools;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let anthropic_api_key =
        std::env::var("ANTHROPIC_API_KEY").wrap_err("ANTHROPIC_API_KEY must be set")?;

    let bridge_url =
        std::env::var("BRIDGE_URL").unwrap_or_else(|_| "http://sidecar:8080".to_string());

    let prompt = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AGENT_PROMPT").ok())
        .unwrap_or_else(|| {
            "List the files in the current directory and describe what you see.".to_string()
        });

    info!(bridge_url, "starting pi agent");

    agent::run(&anthropic_api_key, &bridge_url, &prompt).await?;

    Ok(())
}
