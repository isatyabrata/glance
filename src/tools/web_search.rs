//! `web_search` — call GLM's `web_search_prime` MCP tool over JSON-RPC.
//!
//! Replaces Claude's built-in `WebSearch`. Routed to `https://open.bigmodel.cn`
//! so search results are billed under the user's GLM plan instead of Anthropic.
//!
//! GLM's MCP endpoint is **Streamable HTTP** (Spring WebFlux). One web_search
//! call therefore needs three POSTs:
//!
//! 1. `initialize` — server returns `Mcp-Session-Id` in headers + protocol info
//!    in the SSE body.
//! 2. `notifications/initialized` — required handshake completion (no body
//!    response).
//! 3. `tools/call` — actual search; response arrives as one SSE `data:` frame.
//!
//! The `Mcp-Session-Id` from step 1 must be echoed in steps 2 and 3.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const MCP_ENDPOINT: &str = "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp";
const TOOL_NAME: &str = "web_search_prime";
const TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    /// Result count hint — GLM ignores it, kept for caller ergonomics so a
    /// caller migrating from Claude's `WebSearch` doesn't have to drop the arg.
    #[serde(default)]
    #[allow(dead_code)]
    top_k: Option<u32>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "web_search".into(),
        description:
            "Search the web via GLM's `web_search_prime` MCP. Use this INSTEAD of Claude's \
             built-in WebSearch — results are billed to the user's GLM plan, not Anthropic. \
             Returns markdown-formatted results (title / url / snippet)."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms (≤70 chars recommended)." },
                "top_k": {
                    "type": "integer",
                    "description": "Result count hint (advisory — GLM ignores it but kept for caller ergonomics).",
                    "minimum": 1
                }
            },
            "required": ["query"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { query, .. } = serde_json::from_value(args)?;
    let cfg = crate::config::load_or_default()?;
    let key = cfg.backend.api_key.trim().to_string();
    if key.is_empty() {
        return Ok(CallToolResult::error(
            "[web_search] backend api_key is empty (set GLANCE_API_KEY)".to_string(),
        ));
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .context("build reqwest client")?;

    // Step 1: initialize — extract Mcp-Session-Id from response headers.
    let init_resp = http
        .post(MCP_ENDPOINT)
        .bearer_auth(&key)
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
        .context("web_search initialize POST")?;
    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("[web_search] initialize did not return Mcp-Session-Id"))?;
    // We don't use the body of initialize, but draining keeps the connection clean.
    let _ = init_resp.text().await;

    // Step 2: notifications/initialized — fire-and-forget, no body expected.
    let _ = http
        .post(MCP_ENDPOINT)
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .send()
        .await
        .context("web_search notifications/initialized POST")?;

    // Step 3: tools/call.
    let resp = http
        .post(MCP_ENDPOINT)
        .bearer_auth(&key)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "arguments": { "search_query": query }
            }
        }))
        .send()
        .await
        .context("web_search tools/call POST")?;

    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Ok(CallToolResult::error(format!(
            "[web_search] tools/call returned HTTP {}: {}",
            status,
            raw.chars().take(400).collect::<String>()
        )));
    }

    // SSE response: one or more `data: {...}` lines. We want the first line
    // whose JSON has the matching id (= 2).
    let payload = parse_sse_data_line(&raw, 2).ok_or_else(|| {
        anyhow!(
            "web_search SSE response had no data line (raw head: {})",
            raw.chars().take(200).collect::<String>()
        )
    })?;
    let parsed: McpResponse = serde_json::from_str(&payload)
        .with_context(|| format!("decode MCP data payload: {}", payload))?;

    if let Some(err) = parsed.error {
        return Ok(CallToolResult::error(format!(
            "[web_search] backend error {}: {}",
            err.code, err.message
        )));
    }

    let result = parsed
        .result
        .ok_or_else(|| anyhow!("web_search MCP response missing both result and error"))?;
    let text = result
        .content
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n\n");

    if text.trim().is_empty() {
        return Ok(CallToolResult::text(format!(
            "(no web_search results for `{}`)",
            query
        )));
    }
    Ok(CallToolResult::text(text))
}

/// Pull the first `data: {...}` line whose JSON has the given `id` field.
/// SSE frames look like: `id:1\nevent:message\ndata:{"jsonrpc":...}\n\n`.
/// We accept either `data:` or `data: ` (with optional space).
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
        // Cheap JSON id probe — avoids a full decode just to filter.
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
    fn parses_mcp_success() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"hi"}]}}"#;
        let p: McpResponse = serde_json::from_str(raw).unwrap();
        assert!(p.error.is_none());
        let blocks = p.result.unwrap().content;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_mcp_error() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"nope"}}"#;
        let p: McpResponse = serde_json::from_str(raw).unwrap();
        assert!(p.result.is_none());
        let e = p.error.unwrap();
        assert_eq!(e.code, -32000);
        assert_eq!(e.message, "nope");
    }

    #[test]
    fn picks_data_line_by_id() {
        let raw = "id:1\nevent:message\ndata:{\"id\":1,\"foo\":\"a\"}\n\n\
                   id:2\nevent:message\ndata:{\"id\":2,\"foo\":\"b\"}\n\n";
        assert_eq!(
            parse_sse_data_line(raw, 2),
            Some("{\"id\":2,\"foo\":\"b\"}".to_string())
        );
        assert_eq!(
            parse_sse_data_line(raw, 1),
            Some("{\"id\":1,\"foo\":\"a\"}".to_string())
        );
        assert_eq!(parse_sse_data_line(raw, 99), None);
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
