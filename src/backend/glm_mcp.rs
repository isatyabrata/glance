//! Thin streamable-HTTP MCP client for GLM's remote MCPs (web-search-prime,
//! zread, web-reader).
//!
//! Wire protocol = three sequential POSTs:
//! 1. `initialize` → server returns `Mcp-Session-Id` header + protocol info.
//! 2. `notifications/initialized` → handshake completion (no body).
//! 3. `tools/call` → response is SSE; we parse the `data:` frame whose
//!    JSON has `id == 2`.
//!
//! Returns the concatenated `text` from `result.content[].text` blocks.
//! Used by `tools::web_search` and `tools::repo_explore` (zread fallback).

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 30;

/// Call an MCP `tools/call` on a remote streamable-HTTP MCP server.
///
/// `endpoint` = full URL like `https://open.bigmodel.cn/api/mcp/zread/mcp`.
/// `api_key`  = bearer token (typically `BackendConfig.api_key`).
pub async fn call_tool(
    api_key: &str,
    endpoint: &str,
    tool: &str,
    arguments: Value,
) -> Result<String> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("glm_mcp: api_key is empty"));
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .context("glm_mcp build reqwest client")?;

    // Step 1: initialize.
    let init = http
        .post(endpoint)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "glance", "version": env!("CARGO_PKG_VERSION") }
            }
        }))
        .send()
        .await
        .context("glm_mcp initialize POST")?;
    let session_id = init
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("glm_mcp: initialize did not return Mcp-Session-Id"))?;
    let _ = init.text().await; // drain

    // Step 2: notifications/initialized.
    let _ = http
        .post(endpoint)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .context("glm_mcp notifications/initialized POST")?;

    // Step 3: tools/call.
    let resp = http
        .post(endpoint)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }))
        .send()
        .await
        .context("glm_mcp tools/call POST")?;

    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "glm_mcp {} returned HTTP {}: {}",
            endpoint,
            status,
            raw.chars().take(400).collect::<String>()
        ));
    }
    let payload = parse_sse_data_line(&raw, 2).ok_or_else(|| {
        anyhow!(
            "glm_mcp: SSE response had no data line (raw head: {})",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    let parsed: McpResponse = serde_json::from_str(&payload)
        .with_context(|| format!("glm_mcp decode payload: {}", payload))?;
    if let Some(err) = parsed.error {
        return Err(anyhow!(
            "glm_mcp backend error {}: {}",
            err.code,
            err.message
        ));
    }
    let result = parsed
        .result
        .ok_or_else(|| anyhow!("glm_mcp: response missing both result and error"))?;
    let text = result
        .content
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(text)
}

/// SSE frame parser — find the `data: {...}` line whose JSON has `id == want`.
fn parse_sse_data_line(raw: &str, want_id: i64) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim();
        let payload = line
            .strip_prefix("data:")
            .map(|s| s.trim_start())
            .unwrap_or("");
        if payload.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(payload) {
            if v.get("id").and_then(|x| x.as_i64()) == Some(want_id) {
                return Some(payload.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct McpResponse {
    #[serde(default)]
    result: Option<McpResult>,
    #[serde(default)]
    error: Option<McpError>,
}

#[derive(Debug, Deserialize)]
struct McpResult {
    #[serde(default)]
    content: Vec<McpContentBlock>,
}

#[derive(Debug, Deserialize)]
struct McpContentBlock {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_data_line_by_id() {
        let raw = "id:1\nevent:message\ndata:{\"id\":1,\"foo\":\"a\"}\n\n\
                   id:2\nevent:message\ndata:{\"id\":2,\"foo\":\"b\"}\n\n";
        assert_eq!(
            parse_sse_data_line(raw, 2),
            Some("{\"id\":2,\"foo\":\"b\"}".to_string())
        );
    }

    #[test]
    fn handles_data_with_space() {
        let raw = "data: {\"id\":7,\"x\":1}\n\n";
        assert_eq!(
            parse_sse_data_line(raw, 7),
            Some("{\"id\":7,\"x\":1}".to_string())
        );
    }
}
