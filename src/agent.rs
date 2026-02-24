use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

#[derive(Debug, Serialize)]
struct MessageRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
}

pub async fn run(api_key: &str, bridge_url: &str) -> Result<()> {
    let client = reqwest::Client::new();

    info!("pi agent ready, using bridge at {bridge_url}");

    let messages = vec![Message {
        role: "user".to_string(),
        content: "You are a coding agent. Respond with a brief status.".to_string(),
    }];

    let request = MessageRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        max_tokens: 1024,
        messages,
    };

    debug!("sending request to anthropic api");

    let response = client
        .post(ANTHROPIC_API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await
        .wrap_err("failed to send request to anthropic api")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        warn!(status = %status, body, "anthropic api returned error");
        return Err(color_eyre::eyre::eyre!(
            "anthropic api returned {status}: {body}"
        ));
    }

    let msg: MessageResponse = response
        .json()
        .await
        .wrap_err("failed to parse anthropic api response")?;

    for block in &msg.content {
        if let Some(text) = &block.text {
            info!(text, "agent response");
        }
    }

    if let Some(reason) = &msg.stop_reason {
        debug!(reason, "stop reason");
    }

    Ok(())
}
