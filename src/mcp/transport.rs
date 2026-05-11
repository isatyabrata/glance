//! stdio JSON-RPC loop.
//!
//! Reads one line per request from stdin, dispatches by method, writes one line
//! per response to stdout. Notifications (no `id`) get no response.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::protocol::{
    CallToolParams, CallToolResult, InitializeResult, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, ListToolsResult, ServerCapabilities, ServerInfo, ToolsCapability,
    PROTOCOL_VERSION,
};
use crate::{config, events, mcp_aggregator, tools};

pub async fn run_stdio() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("invalid JSON-RPC: {} (raw: {})", e, line);
                continue;
            }
        };

        // Notifications: no id → no response, side effects only.
        if req.id.is_none() {
            tracing::debug!("notification: {}", req.method);
            continue;
        }

        let resp = dispatch(req).await;
        let line = serde_json::to_string(&resp)? + "\n";
        stdout.write_all(line.as_bytes()).await?;
        stdout.flush().await?;
    }

    tracing::info!("stdin closed, glance-mcp exiting");
    Ok(())
}

async fn dispatch(req: JsonRpcRequest) -> JsonRpcResponse {
    let id = req.id.clone().unwrap_or(Value::Null);
    match req.method.as_str() {
        "initialize" => {
            // Capture clientInfo.name so the aggregator can apply per-client
            // allowlists (e.g. chrome-devtools exposed only to claude).
            if let Some(name) = req
                .params
                .as_ref()
                .and_then(|p| p.get("clientInfo"))
                .and_then(|c| c.get("name"))
                .and_then(|n| n.as_str())
            {
                super::record_client(name);
                tracing::info!(client = %super::current_client(), raw = name, "MCP client identified");
            }
            match handle_initialize() {
                Ok(v) => JsonRpcResponse::ok(id, v),
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
            }
        }
        "tools/list" => match handle_list_tools().await {
            Ok(v) => JsonRpcResponse::ok(id, v),
            Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
        },
        "tools/call" => {
            let params: CallToolParams = match req
                .params
                .clone()
                .map(serde_json::from_value::<CallToolParams>)
                .transpose()
            {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return JsonRpcResponse::err(
                        id,
                        JsonRpcError::invalid_params("missing params"),
                    );
                }
                Err(e) => {
                    return JsonRpcResponse::err(
                        id,
                        JsonRpcError::invalid_params(format!("bad params: {}", e)),
                    );
                }
            };
            match handle_call_tool(params).await {
                Ok(v) => JsonRpcResponse::ok(id, v),
                Err(e) => JsonRpcResponse::err(id, JsonRpcError::internal(e.to_string())),
            }
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        other => JsonRpcResponse::err(id, JsonRpcError::method_not_found(other)),
    }
}

fn handle_initialize() -> Result<Value> {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        server_info: ServerInfo {
            name: "glance",
            version: env!("CARGO_PKG_VERSION"),
        },
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: Some(false),
            }),
        },
    };
    Ok(serde_json::to_value(result)?)
}

async fn handle_list_tools() -> Result<Value> {
    let mut tools = tools::list_enabled().await?;
    if let Some(agg) = mcp_aggregator::current().await {
        tools.extend(agg.list_tools().await);
    }
    Ok(serde_json::to_value(ListToolsResult { tools })?)
}

async fn handle_call_tool(params: CallToolParams) -> Result<Value> {
    // Capture flag + args before we move them into the dispatcher.
    let events_enabled = config::load_or_default()
        .map(|c| c.events_enabled)
        .unwrap_or(false);
    let tool_name = params.name.clone();
    let args_value = params.arguments.clone();
    let started = std::time::Instant::now();

    // Per-call accounting cell visible to sub-agent loops via task_local.
    let ctx = std::sync::Arc::new(events::CallCtx::default());
    let ctx_for_scope = ctx.clone();
    let args_for_dispatch = params.arguments;
    let name_for_dispatch = params.name;
    // Wall-clock guard so glance always responds within the MCP client's
    // 120 s tools/call ceiling. If any dispatch path hangs we return a clean
    // timeout error instead of leaving the calling LLM stranded.
    const TOOL_DISPATCH_TIMEOUT_SECS: u64 = 110;
    let dispatch_future = events::CALL_CTX.scope(ctx_for_scope, async move {
        if let Some(agg) = mcp_aggregator::current().await {
            match agg
                .call_tool(&name_for_dispatch, args_for_dispatch.clone())
                .await
            {
                Ok(Some(r)) => return Ok(r),
                Ok(None) => {} // not an aggregator tool — fall through
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "aggregator dispatch error: {}",
                        e
                    )));
                }
            }
        }
        tools::dispatch(&name_for_dispatch, args_for_dispatch).await
    });
    let dispatch_result = match tokio::time::timeout(
        std::time::Duration::from_secs(TOOL_DISPATCH_TIMEOUT_SECS),
        dispatch_future,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Ok(CallToolResult::error(format!(
            "tool `{}` exceeded glance's {}s wall-clock guard — upstream is hung. Try narrowing scope.",
            tool_name, TOOL_DISPATCH_TIMEOUT_SECS
        ))),
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let (result, ok, error_msg) = match dispatch_result {
        Ok(r) => {
            let is_err = r.is_error.unwrap_or(false);
            (r, !is_err, None)
        }
        Err(e) => {
            let msg = format!("{}", e);
            (
                CallToolResult::error(format!("tool error: {}", msg)),
                false,
                Some(msg),
            )
        }
    };

    // Bytes returned to the caller = sum of text-block lengths in the response.
    let bytes_out: u64 = result
        .content
        .iter()
        .map(|b| match b {
            super::protocol::ToolContentBlock::Text { text } => text.len() as u64,
        })
        .sum();
    let bytes_in = ctx.snapshot_in();
    let (glm_prompt, glm_completion, glm_iters) = ctx.snapshot_glm();
    let (glm_cached, glm_cache_creation) = ctx.snapshot_cache();
    // u64 -> u32: GLM token totals are well under 4B per MCP call, but we
    // saturate just to be safe rather than wrap.
    let token_acct = events::TokenAccounting {
        prompt: u32::try_from(glm_prompt).unwrap_or(u32::MAX),
        completion: u32::try_from(glm_completion).unwrap_or(u32::MAX),
        iters: glm_iters,
        cached: u32::try_from(glm_cached).unwrap_or(u32::MAX),
        cache_creation: u32::try_from(glm_cache_creation).unwrap_or(u32::MAX),
    };

    events::record(
        events_enabled,
        &tool_name,
        &args_value,
        token_acct,
        duration_ms,
        ok,
        error_msg,
        events::ByteAccounting {
            bytes_in,
            bytes_out,
        },
    );

    Ok(serde_json::to_value(result)?)
}
