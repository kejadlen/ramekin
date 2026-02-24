use std::time::Duration;

use axum::{Json, Router, http::StatusCode, routing::get};
use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Deserialize)]
struct ProxyRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ProxyResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: serde_json::Value,
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

async fn proxy(Json(req): Json<ProxyRequest>) -> Result<Json<ProxyResponse>, (StatusCode, String)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut builder = match req.method.to_uppercase().as_str() {
        "GET" => client.get(&req.url),
        "POST" => client.post(&req.url),
        "PUT" => client.put(&req.url),
        "PATCH" => client.patch(&req.url),
        "DELETE" => client.delete(&req.url),
        "HEAD" => client.head(&req.url),
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unsupported method: {other}"),
            ));
        }
    };

    if let Some(headers) = req.headers {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
    }

    if let Some(body) = req.body {
        builder = builder.json(&body);
    }

    info!(method = %req.method, url = %req.url, "proxying request");

    let response = builder
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("proxy error: {e}")))?;

    let status = response.status().as_u16();

    let resp_headers: std::collections::HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();

    let resp_body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

    info!(status, "proxied response");

    Ok(Json(ProxyResponse {
        status,
        headers: resp_headers,
        body: resp_body,
    }))
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

    let app = Router::new()
        .route("/health", get(health))
        .route("/proxy", axum::routing::post(proxy));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, "bridge server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
