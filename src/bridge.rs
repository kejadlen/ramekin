/// Client-side helpers for communicating with the sidecar bridge server.
///
/// The bridge server runs in the sidecar container and acts as an intermediary
/// for any communication to the outside world beyond the Anthropic API.

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Serialize)]
pub struct BridgeRequest {
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct BridgeResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body: serde_json::Value,
}

pub struct BridgeClient {
    base_url: String,
    client: reqwest::Client,
}

impl BridgeClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Forward an HTTP request through the bridge server.
    pub async fn forward(&self, request: BridgeRequest) -> Result<BridgeResponse> {
        debug!(method = %request.method, url = %request.url, "forwarding request through bridge");

        let response = self
            .client
            .post(format!("{}/proxy", self.base_url))
            .json(&request)
            .send()
            .await
            .wrap_err("failed to reach bridge server")?;

        let bridge_resp: BridgeResponse = response
            .json()
            .await
            .wrap_err("failed to parse bridge response")?;

        debug!(status = bridge_resp.status, "bridge response received");

        Ok(bridge_resp)
    }

    /// Health check for the bridge server.
    pub async fn health(&self) -> Result<bool> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .wrap_err("bridge health check failed")?;

        Ok(resp.status().is_success())
    }
}
