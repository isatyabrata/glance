//! Streamable-HTTP MCP client.
//!
//! Used for upstreams that expose JSON-RPC over a single HTTPS endpoint with
//! SSE responses (GLM-style: `Mcp-Session-Id` header from `initialize`,
//! reused for every subsequent request).
//!
//! Behaviour matches `src/backend/glm_mcp.rs::call_tool` but lives in a
//! reusable client struct that supports both `tools/list` and `tools/call`.
//! `glm_mcp.rs` keeps its tiny `call_tool` wrapper so existing callers
//! (`tools/web_search.rs`) compile unchanged.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::mcp::protocol::{ToolContentBlock, ToolDefinition};

const TIMEOUT_SECS: u64 = 100;
const PROTOCOL_VERSION: &str = "2024-11-05";

pub struct StreamableHttpClient {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
    /// Cached session id from the most recent `initialize`. We re-init lazily
    /// on the first call. Wrapped in an `async` Mutex so `list_tools` and
    /// concurrent `call_tool`s serialize their re-init attempt.
    session_id: Mutex<Option<String>>,
    /// JSON-RPC id counter (starts at 100 to dodge handshake ids 1/2).
    next_id: AtomicI64,
    /// Init counter — used so concurrent callers can detect a stale session.
    #[allow(dead_code)]
    init_epoch: AtomicU64,
    /// Last-known protocol version returned by the server (for debugging).
    server_protocol: StdMutex<Option<String>>,
}

impl StreamableHttpClient {
    pub fn new(endpoint: &str, api_key: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()
            .expect("reqwest client build");
        Self {
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            http,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(100),
            init_epoch: AtomicU64::new(0),
            server_protocol: StdMutex::new(None),
        }
    }

    /// Ensure we have a session id. If not, run the `initialize` +
    /// `notifications/initialized` handshake.
    async fn ensure_session(&self) -> Result<String> {
        {
            let g = self.session_id.lock().await;
            if let Some(s) = g.as_ref() {
                return Ok(s.clone());
            }
        }
        let init_resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "glance-aggregator", "version": env!("CARGO_PKG_VERSION") }
                }
            }))
            .send()
            .await
            .with_context(|| format!("initialize POST to {}", self.endpoint))?;
        let session_id = init_resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!(
                    "initialize did not return Mcp-Session-Id ({} status {})",
                    self.endpoint,
                    init_resp.status()
                )
            })?;
        // Drain init body — also try to record the protocol version for diagnostics.
        if let Ok(body) = init_resp.text().await {
            if let Some(payload) = parse_sse_data_line(&body, 1) {
                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                    if let Some(p) = v
                        .get("result")
                        .and_then(|r| r.get("protocolVersion"))
                        .and_then(|x| x.as_str())
                    {
                        if let Ok(mut g) = self.server_protocol.lock() {
                            *g = Some(p.to_string());
                        }
                    }
                }
            }
        }

        // Fire-and-forget initialized notification.
        let _ = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await
            .context("notifications/initialized")?;

        let mut g = self.session_id.lock().await;
        *g = Some(session_id.clone());
        self.init_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(session_id)
    }

    /// `tools/list` against the upstream.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let session = self.ensure_session().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/list",
                "params": {}
            }))
            .send()
            .await
            .context("tools/list POST")?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "tools/list HTTP {}: {}",
                status,
                raw.chars().take(400).collect::<String>()
            ));
        }
        let payload = parse_sse_data_line(&raw, id).ok_or_else(|| {
            anyhow!(
                "tools/list SSE response had no data line for id {} (raw head: {})",
                id,
                raw.chars().take(200).collect::<String>()
            )
        })?;
        let v: Value = serde_json::from_str(&payload).context("decode tools/list payload")?;
        if let Some(e) = v.get("error") {
            return Err(anyhow!("tools/list error: {}", e));
        }
        let tools = v
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .ok_or_else(|| anyhow!("tools/list missing result.tools"))?;
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            let name = t
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| anyhow!("tool entry missing name"))?
                .to_string();
            let description = t
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            out.push(ToolDefinition {
                name,
                description,
                input_schema,
            });
        }
        Ok(out)
    }

    /// `tools/call` against the upstream.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<Vec<ToolContentBlock>> {
        let session = self.ensure_session().await?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .send()
            .await
            .with_context(|| format!("tools/call {}", tool))?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "tools/call HTTP {}: {}",
                status,
                raw.chars().take(400).collect::<String>()
            ));
        }
        let payload = parse_sse_data_line(&raw, id).ok_or_else(|| {
            anyhow!(
                "tools/call SSE response had no data line for id {} (raw head: {})",
                id,
                raw.chars().take(200).collect::<String>()
            )
        })?;
        let v: Value = serde_json::from_str(&payload).context("decode tools/call payload")?;
        if let Some(e) = v.get("error") {
            return Err(anyhow!("upstream tools/call error: {}", e));
        }
        let content = v
            .get("result")
            .and_then(|r| r.get("content"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let arr = content.as_array().cloned().unwrap_or_default();
        let mut blocks = Vec::with_capacity(arr.len());
        for block in arr {
            let kind = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if kind == "text" {
                let text = block
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ToolContentBlock::Text { text });
            } else {
                blocks.push(ToolContentBlock::Text {
                    text: format!("[non-text content type=`{}` dropped: {}]", kind, block),
                });
            }
        }
        if blocks.is_empty() {
            blocks.push(ToolContentBlock::Text {
                text: "(upstream returned empty content)".into(),
            });
        }
        Ok(blocks)
    }
}

/// Same SSE parser as `glm_mcp.rs` — find the first `data: {...}` line whose
/// payload has the requested `id` field.
pub(crate) fn parse_sse_data_line(raw: &str, want_id: i64) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_picks_correct_id() {
        let raw = "data: {\"id\":42,\"result\":{}}\n\n";
        assert_eq!(
            parse_sse_data_line(raw, 42),
            Some("{\"id\":42,\"result\":{}}".to_string())
        );
        assert_eq!(parse_sse_data_line(raw, 99), None);
    }
}
