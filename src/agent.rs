use color_eyre::eyre::{Result, WrapErr};
use tracing::{debug, info, warn};

use crate::tools;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const MODEL: &str = "claude-sonnet-4-20250514";
const MAX_TOKENS: u32 = 4096;
const MAX_ITERATIONS: u32 = 200;

const SYSTEM_PROMPT: &str = "\
You are a coding agent running in a sandboxed container. You have access to \
tools for executing bash commands, reading files, writing files, and listing \
directory contents. Use these tools to accomplish the task given to you.

Work step by step. Think about what you need to do, use the available tools, \
and iterate until the task is complete. When you are done, provide a brief \
summary of what you accomplished.";

pub async fn run(api_key: &str, _bridge_url: &str, prompt: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let tool_defs = tools::definitions();

    info!("pi agent ready, starting agentic loop");

    let mut messages: Vec<serde_json::Value> = vec![serde_json::json!({
        "role": "user",
        "content": prompt,
    })];

    for iteration in 0..MAX_ITERATIONS {
        debug!(iteration, "sending request to anthropic api");

        let request_body = serde_json::json!({
            "model": MODEL,
            "max_tokens": MAX_TOKENS,
            "system": SYSTEM_PROMPT,
            "tools": tool_defs,
            "messages": messages,
        });

        let response = client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request_body)
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

        let response_body: serde_json::Value = response
            .json()
            .await
            .wrap_err("failed to parse anthropic api response")?;

        let content = &response_body["content"];
        let stop_reason = response_body["stop_reason"].as_str().unwrap_or("");

        // Log text blocks from the response.
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block["type"] == "text" {
                    if let Some(text) = block["text"].as_str() {
                        info!("{text}");
                    }
                }
            }
        }

        // Append the assistant message to the conversation.
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": content,
        }));

        if stop_reason != "tool_use" {
            info!(stop_reason, "agent finished");
            break;
        }

        // Execute each tool_use block and collect results.
        let mut tool_results: Vec<serde_json::Value> = Vec::new();

        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block["type"] != "tool_use" {
                    continue;
                }

                let id = block["id"].as_str().unwrap_or("");
                let name = block["name"].as_str().unwrap_or("");
                let input = &block["input"];

                info!(name, "executing tool");

                let result = match name {
                    "bash" => {
                        let cmd = input["command"].as_str().unwrap_or("");
                        tools::execute_bash(cmd).await
                    }
                    "read_file" => {
                        let path = input["path"].as_str().unwrap_or("");
                        tools::read_file(path).await
                    }
                    "write_file" => {
                        let path = input["path"].as_str().unwrap_or("");
                        let file_content = input["content"].as_str().unwrap_or("");
                        tools::write_file(path, file_content).await
                    }
                    "list_files" => {
                        let path = input["path"].as_str().unwrap_or(".");
                        tools::list_files(path).await
                    }
                    _ => format!("Unknown tool: {name}"),
                };

                debug!(name, result_len = result.len(), "tool result");

                tool_results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": result,
                }));
            }
        }

        // Append the tool results as a user message.
        messages.push(serde_json::json!({
            "role": "user",
            "content": tool_results,
        }));
    }

    Ok(())
}
