//! Stdio MCP client.
//!
//! Spawns an MCP server as a subprocess, speaks JSON-RPC line-delimited over
//! its stdin/stdout. One reader task drains stdout, parses each line as
//! JSON-RPC, and dispatches the response to the matching pending request via
//! a `oneshot` channel keyed by request id.
//!
//! Stderr is forwarded to `tracing::warn!` so misbehaving upstreams are
//! debuggable from glance's logs without polluting the JSON-RPC stream.
//!
//! On `Drop` the child is killed (SIGKILL via tokio's drop-handle).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::mcp::protocol::{ToolContentBlock, ToolDefinition};

const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
const TOOL_CALL_TIMEOUT_SECS: u64 = 100;
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Map of pending requests, keyed by JSON-RPC id.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct StdioMcpClient {
    /// Atomic counter for request ids. Starts at 100 because we use 1 + 2 for
    /// the synchronous handshake.
    next_id: AtomicU64,
    /// Serialized writer to the child's stdin (one writer at a time).
    stdin: Mutex<ChildStdin>,
    /// Pending response slots — keyed by request id.
    pending: PendingMap,
    /// Owned subprocess handle. Drop = kill.
    #[allow(dead_code)]
    child: Mutex<Child>,
    /// Flipped to `false` once the reader sees EOF, the writer hits a broken
    /// pipe, or we proactively detect the child has exited via `try_wait`.
    /// Cheap to read on every `call_tool` (one atomic load).
    alive: Arc<AtomicBool>,
}

impl StdioMcpClient {
    /// Spawn an MCP server, do the `initialize` + `notifications/initialized`
    /// handshake, return the live client.
    pub async fn start(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn `{}`", command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("child stderr not captured"))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        // Stdout reader: dispatch responses by id.
        {
            let pending = pending.clone();
            let alive = alive.clone();
            let cmd_label = command.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            let v: Value = match serde_json::from_str(line) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!(
                                        "stdio[{}] non-JSON line ignored ({}): {}",
                                        cmd_label,
                                        e,
                                        line.chars().take(120).collect::<String>()
                                    );
                                    continue;
                                }
                            };
                            // Notifications (no id) — ignore.
                            let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                                continue;
                            };
                            let tx = {
                                let mut g = pending.lock().await;
                                g.remove(&id)
                            };
                            if let Some(tx) = tx {
                                let _ = tx.send(v);
                            } else {
                                tracing::debug!(
                                    "stdio[{}] response for unknown id {} dropped",
                                    cmd_label,
                                    id
                                );
                            }
                        }
                        Ok(None) => {
                            tracing::info!(
                                "stdio[{}] stdout closed (child likely exited)",
                                cmd_label
                            );
                            alive.store(false, Ordering::SeqCst);
                            // Wake any pending callers so they don't time out.
                            let mut g = pending.lock().await;
                            g.clear();
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("stdio[{}] read error: {}", cmd_label, e);
                            alive.store(false, Ordering::SeqCst);
                            let mut g = pending.lock().await;
                            g.clear();
                            break;
                        }
                    }
                }
            });
        }

        // Stderr → tracing.
        {
            let cmd_label = command.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        tracing::warn!("stdio[{}][stderr] {}", cmd_label, trimmed);
                    }
                }
            });
        }

        let client = Self {
            next_id: AtomicU64::new(100),
            stdin: Mutex::new(stdin),
            pending,
            child: Mutex::new(child),
            alive,
        };

        // Handshake: id=1 initialize, then notifications/initialized.
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "glance-aggregator", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let _init_resp = client
            .request_with_id(1, init_req, HANDSHAKE_TIMEOUT_SECS)
            .await
            .context("MCP initialize handshake")?;

        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        client
            .send_notification(notif)
            .await
            .context("MCP notifications/initialized")?;

        Ok(client)
    }

    /// `tools/list` — returns the upstream's raw tools (no namespacing).
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        });
        let resp = self
            .request_with_id(id, req, HANDSHAKE_TIMEOUT_SECS)
            .await
            .context("tools/list")?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("tools/list error: {}", err));
        }
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .cloned()
            .ok_or_else(|| anyhow!("tools/list response missing result.tools"))?;
        let arr = tools
            .as_array()
            .ok_or_else(|| anyhow!("tools/list result.tools is not an array"))?;
        let mut out = Vec::with_capacity(arr.len());
        for t in arr {
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

    /// `tools/call` — returns the raw `content` blocks from the upstream's
    /// response (text only — non-text blocks are dropped with a placeholder).
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<Vec<ToolContentBlock>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments
            }
        });
        let resp = self
            .request_with_id(id, req, TOOL_CALL_TIMEOUT_SECS)
            .await
            .with_context(|| format!("tools/call {}", tool))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("upstream tools/call error: {}", err));
        }
        let content = resp
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
                // Non-text content (image, resource, etc) — render as a
                // placeholder so the model still gets a hint. Keep the JSON
                // so the user can see what was filtered.
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

    /// Cheap liveness check, called by the aggregator before every dispatch.
    ///
    /// Combines two signals:
    /// 1. The `alive` atomic, flipped by the stdout reader on EOF and by the
    ///    write path on `BrokenPipe`. This is the fast path (one atomic load).
    /// 2. A best-effort `try_wait()` on the child handle if the lock is
    ///    immediately available. Catches the case where the child exited but
    ///    no IO has happened yet to flip `alive`.
    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        // Non-blocking probe — if the lock is held by another task we just
        // trust the atomic. Avoids any contention with `call_tool`.
        if let Ok(mut child) = self.child.try_lock() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    tracing::warn!("stdio child exited with status {:?}", status);
                    self.alive.store(false, Ordering::SeqCst);
                    false
                }
                Err(e) => {
                    tracing::warn!("stdio try_wait error: {} (treating as dead)", e);
                    self.alive.store(false, Ordering::SeqCst);
                    false
                }
            }
        } else {
            true
        }
    }

    /// Mark the client dead. Used by the writer path on `BrokenPipe` to
    /// short-circuit further requests until the aggregator respawns us.
    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    /// Internal: send a request with a pre-allocated id, block until the
    /// reader task hands us back the response (or timeout).
    async fn request_with_id(&self, id: u64, body: Value, timeout_secs: u64) -> Result<Value> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(anyhow!("stdio child is no longer alive"));
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut g = self.pending.lock().await;
            g.insert(id, tx);
        }
        let line = serde_json::to_string(&body)? + "\n";
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                if e.kind() == ErrorKind::BrokenPipe {
                    self.mark_dead();
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!("stdio child closed stdin (broken pipe)"));
                }
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("write to stdin: {}", e));
            }
            if let Err(e) = stdin.flush().await {
                if e.kind() == ErrorKind::BrokenPipe {
                    self.mark_dead();
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!("stdio child closed stdin during flush"));
                }
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("flush stdin: {}", e));
            }
        }
        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => {
                // Reader closed the oneshot — almost always means the child
                // died and the reader cleared the pending map.
                self.mark_dead();
                Err(anyhow!("response channel closed (child likely died)"))
            }
            Err(_) => {
                // Timed out — clean up the pending slot so we don't leak.
                let mut g = self.pending.lock().await;
                g.remove(&id);
                Err(anyhow!(
                    "upstream request timed out after {}s",
                    timeout_secs
                ))
            }
        }
    }

    async fn send_notification(&self, body: Value) -> Result<()> {
        let line = serde_json::to_string(&body)? + "\n";
        let mut stdin = self.stdin.lock().await;
        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            if e.kind() == ErrorKind::BrokenPipe {
                self.mark_dead();
            }
            return Err(anyhow!("write notification: {}", e));
        }
        if let Err(e) = stdin.flush().await {
            if e.kind() == ErrorKind::BrokenPipe {
                self.mark_dead();
            }
            return Err(anyhow!("flush notification: {}", e));
        }
        Ok(())
    }
}
