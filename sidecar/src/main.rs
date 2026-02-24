use axum::{Json, Router, routing::post};
use color_eyre::eyre::Result;
use tracing::info;

async fn echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(body)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let port: u16 = std::env::var("BRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let app = Router::new().route("/echo", post(echo));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, "bridge server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
