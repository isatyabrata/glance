//! MCP aggregator — glance hosts external MCP servers as upstream proxies.
//!
//! Each enabled entry in `config.upstream_mcps` is connected on startup
//! (stdio subprocess or streamable-HTTP). Their `tools/list` is merged into
//! glance's own list under a `<upstream_name>__` namespace prefix; calls to
//! `<name>__<tool>` are routed to the matching upstream.
//!
//! Failures are non-fatal: an upstream that fails to start is recorded with
//! its error and contributes zero tools — glance's own 17 tools still work.

pub mod client_stdio;
pub mod client_streamable_http;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

use crate::config::UpstreamMcp;
use crate::mcp::protocol::{CallToolResult, ToolContentBlock, ToolDefinition};

/// How many consecutive respawn failures before we give up on a stdio
/// upstream (until cooldown elapses or the GUI manually retries).
const MAX_CONSECUTIVE_RESPAWN_FAILURES: u32 = 3;
/// After hitting the respawn-failure budget we wait this long before trying
/// again on the next tool call.
const RESPAWN_FAILURE_COOLDOWN_SECS: u64 = 60;

/// The aggregator namespace separator. Tool names exposed to MCP clients are
/// `<upstream_name>__<remote_tool_name>`. We only split on the *first* `__`
/// from the left; remote tools may legitimately contain `_` (and even `__`,
/// though we avoid recommending it).
pub const NAMESPACE_SEP: &str = "__";

/// Per-upstream runtime state. Behind an `Arc` so the read-side (list_tools /
/// call_tool) can clone cheaply.
pub struct UpstreamState {
    pub name: String,
    pub type_label: &'static str,
    /// Mutable status — flipped to `Failed` if the stdio respawn budget is
    /// exhausted. Behind a `RwLock` so `call_tool` can promote a transient
    /// death into a sticky failure without taking `&mut self`.
    pub status: RwLock<UpstreamStatus>,
    /// Tool definitions as returned by the upstream's `tools/list` (with
    /// names already prefixed with `<name>__`).
    pub tools: Vec<ToolDefinition>,
    /// Last error message if `status == Failed`. Mutable so respawn paths
    /// can update the GUI-visible reason without a process restart.
    pub last_error: RwLock<Option<String>>,
    /// Connection-time latency in ms (initialize + tools/list round-trip).
    pub connect_ms: Option<u64>,
    /// Per-client allowlist (carried from `UpstreamMcp.clients`). Empty =
    /// expose to all clients. Non-empty = filter list_tools / call_tool.
    pub clients: Vec<String>,
    /// Transport handle. `None` for failed / disabled upstreams.
    transport: RwLock<Option<Transport>>,
    /// Original spec — kept so we can respawn stdio upstreams on death.
    spec: UpstreamMcp,
    /// Carried from the parent `Aggregator::start` call so HTTP upstreams
    /// resolve their bearer token the same way on every respawn (unused for
    /// stdio today, but stored for symmetry / future use).
    fallback_api_key: String,
    /// Number of consecutive respawn attempts that have failed. Reset to 0
    /// after a successful respawn or once the cooldown elapses.
    consecutive_respawn_failures: AtomicU32,
    /// Wall-clock instant of the last respawn failure, used for the 60 s
    /// cooldown. `None` means we haven't given up yet.
    last_failure_at: Mutex<Option<Instant>>,
}

impl UpstreamState {
    /// Whether this upstream is exposed to the given normalized client id.
    pub fn allowed_for(&self, client: &str) -> bool {
        if self.clients.is_empty() {
            return true;
        }
        self.clients.iter().any(|c| c.eq_ignore_ascii_case(client))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamStatus {
    Connected,
    Failed,
    Disabled,
}

#[derive(Clone)]
enum Transport {
    Stdio(Arc<client_stdio::StdioMcpClient>),
    Http(Arc<client_streamable_http::StreamableHttpClient>),
}

/// The aggregator. Holds per-upstream state. Owned via an `RwLock` so the
/// GUI's add/remove commands can swap it out without process restart.
pub struct Aggregator {
    states: Vec<UpstreamState>,
}

impl Aggregator {
    /// Spawn / connect every enabled upstream in parallel. Failures don't
    /// abort — the aggregator returns with whatever succeeded.
    pub async fn start(specs: &[UpstreamMcp], fallback_api_key: &str) -> Self {
        let mut tasks = Vec::with_capacity(specs.len());
        for spec in specs {
            let spec = spec.clone();
            let fallback_api_key = fallback_api_key.to_string();
            tasks.push(tokio::spawn(async move {
                connect_one(spec, fallback_api_key).await
            }));
        }
        let mut states = Vec::with_capacity(tasks.len());
        for t in tasks {
            match t.await {
                Ok(state) => states.push(state),
                Err(e) => {
                    tracing::warn!("aggregator: connect task panicked: {}", e);
                }
            }
        }
        Self { states }
    }

    /// Empty aggregator — no upstreams configured.
    pub fn empty() -> Self {
        Self { states: Vec::new() }
    }

    /// Concatenate all connected upstreams' tools (already namespaced).
    /// Skips upstreams whose `clients` allowlist excludes the current MCP
    /// client (`crate::mcp::current_client()`).
    pub async fn list_tools(&self) -> Vec<ToolDefinition> {
        let client = crate::mcp::current_client();
        let mut out = Vec::new();
        for s in &self.states {
            if !matches!(*s.status.read().await, UpstreamStatus::Connected) {
                continue;
            }
            if !s.allowed_for(client) {
                continue;
            }
            out.extend(s.tools.iter().cloned());
        }
        out
    }

    /// Snapshot of every upstream's status — for the GUI.
    pub async fn status_snapshot(&self) -> Vec<UpstreamStatusSnapshot> {
        let current = crate::mcp::current_client();
        let mut out = Vec::with_capacity(self.states.len());
        for s in &self.states {
            out.push(UpstreamStatusSnapshot {
                name: s.name.clone(),
                type_label: s.type_label,
                status: *s.status.read().await,
                tool_count: s.tools.len(),
                last_error: s.last_error.read().await.clone(),
                connect_ms: s.connect_ms,
                clients: s.clients.clone(),
                exposed_to_current: s.allowed_for(current),
            });
        }
        out
    }

    /// Route a `tools/call`. Returns `Ok(None)` if `tool_name` doesn't have a
    /// `<known-upstream>__` prefix — caller falls through to glance's own
    /// built-in tool dispatcher.
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<Option<CallToolResult>> {
        let Some((upstream_name, remote_tool)) = split_namespaced(tool_name) else {
            return Ok(None);
        };
        let Some(state) = self.states.iter().find(|s| s.name == upstream_name) else {
            return Ok(None);
        };
        let client = crate::mcp::current_client();
        if !state.allowed_for(client) {
            return Ok(Some(CallToolResult::error(format!(
                "[aggregator] upstream `{}` is not exposed to client `{}` (configured `clients = {:?}`)",
                upstream_name, client, state.clients
            ))));
        }
        // Read current status under a short-lived lock; bail early on
        // disabled / sticky-failed upstreams.
        match *state.status.read().await {
            UpstreamStatus::Connected => {}
            UpstreamStatus::Failed => {
                // Failed stdio upstreams may become eligible for a retry once
                // the cooldown elapses — check that before refusing.
                if !matches!(state.spec, UpstreamMcp::Stdio { .. })
                    || !respawn_cooldown_elapsed(state).await
                {
                    let detail = state
                        .last_error
                        .read()
                        .await
                        .clone()
                        .unwrap_or_else(|| "(no detail)".into());
                    return Ok(Some(CallToolResult::error(format!(
                        "[aggregator] upstream `{}` is in failed state: {}",
                        upstream_name, detail
                    ))));
                }
                tracing::info!(
                    upstream = %upstream_name,
                    "respawn cooldown elapsed, attempting another respawn"
                );
            }
            UpstreamStatus::Disabled => {
                return Ok(Some(CallToolResult::error(format!(
                    "[aggregator] upstream `{}` is disabled",
                    upstream_name
                ))));
            }
        }

        // Stdio path: check liveness, respawn if dead. Done before grabbing
        // the transport snapshot so we always dispatch through a live handle.
        if matches!(state.spec, UpstreamMcp::Stdio { .. }) {
            let dead = matches!(
                state.transport.read().await.as_ref(),
                Some(Transport::Stdio(c)) if !c.is_alive()
            ) || state.transport.read().await.is_none();
            if dead {
                if let Err(e) = self.respawn_stdio(state).await {
                    return Ok(Some(CallToolResult::error(format!(
                        "[aggregator] upstream `{}` respawn failed: {}",
                        upstream_name, e
                    ))));
                }
            }
        }

        let transport_snapshot = state.transport.read().await.as_ref().cloned();
        let Some(transport) = transport_snapshot else {
            return Ok(Some(CallToolResult::error(format!(
                "[aggregator] upstream `{}` has no transport handle",
                upstream_name
            ))));
        };
        let result = match &transport {
            Transport::Stdio(c) => c.call_tool(remote_tool, args).await,
            Transport::Http(c) => c.call_tool(remote_tool, args).await,
        };
        Ok(Some(match result {
            Ok(blocks) => CallToolResult {
                content: blocks,
                is_error: None,
            },
            Err(e) => CallToolResult::error(format!(
                "[aggregator] {}__{} call failed: {}",
                upstream_name, remote_tool, e
            )),
        }))
    }

    /// Rebuild the stdio transport for a single upstream after detecting that
    /// the child died. Bounded by `MAX_CONSECUTIVE_RESPAWN_FAILURES`; once the
    /// budget is spent the upstream is marked `Failed` until the cooldown
    /// elapses or the GUI's test/reload triggers a manual rebuild.
    async fn respawn_stdio(&self, state: &UpstreamState) -> Result<()> {
        let attempts_before = state.consecutive_respawn_failures.load(Ordering::SeqCst);
        if attempts_before >= MAX_CONSECUTIVE_RESPAWN_FAILURES {
            anyhow::bail!(
                "respawn budget exhausted ({} consecutive failures)",
                attempts_before
            );
        }
        tracing::warn!(
            upstream = %state.name,
            attempt = attempts_before + 1,
            "stdio upstream died, respawning"
        );
        match build_transport(&state.spec, &state.fallback_api_key).await {
            Ok((transport, _tools)) => {
                // tools list is unchanged — keep the prefixed names already on
                // `state.tools` so we don't disturb GUI snapshots mid-recovery.
                *state.transport.write().await = Some(transport);
                *state.status.write().await = UpstreamStatus::Connected;
                *state.last_error.write().await = None;
                state
                    .consecutive_respawn_failures
                    .store(0, Ordering::SeqCst);
                *state.last_failure_at.lock().await = None;
                tracing::info!(upstream = %state.name, "stdio upstream respawned successfully");
                Ok(())
            }
            Err(e) => {
                let n = state
                    .consecutive_respawn_failures
                    .fetch_add(1, Ordering::SeqCst)
                    + 1;
                *state.last_error.write().await = Some(format!("respawn: {}", e));
                if n >= MAX_CONSECUTIVE_RESPAWN_FAILURES {
                    *state.status.write().await = UpstreamStatus::Failed;
                    *state.last_failure_at.lock().await = Some(Instant::now());
                    tracing::error!(
                        upstream = %state.name,
                        failures = n,
                        cooldown_secs = RESPAWN_FAILURE_COOLDOWN_SECS,
                        "upstream failed {}× in a row, giving up until cooldown",
                        n
                    );
                } else {
                    tracing::warn!(
                        upstream = %state.name,
                        attempt = n,
                        err = %e,
                        "respawn attempt failed"
                    );
                }
                Err(e)
            }
        }
    }
}

/// True if the upstream's last sticky-failure was more than the cooldown ago,
/// in which case `call_tool` will reset the failure counter and try once more.
async fn respawn_cooldown_elapsed(state: &UpstreamState) -> bool {
    let mut slot = state.last_failure_at.lock().await;
    let elapsed = match *slot {
        Some(at) => at.elapsed().as_secs() >= RESPAWN_FAILURE_COOLDOWN_SECS,
        None => false,
    };
    if elapsed {
        // Reset the budget so the next call gets a fresh chance.
        state
            .consecutive_respawn_failures
            .store(0, Ordering::SeqCst);
        *slot = None;
    }
    elapsed
}

/// Static slot the stdio MCP entry-point and the GUI both write through. We
/// use `RwLock<Option<Aggregator>>` (not `OnceLock`) so add/remove commands
/// can rebuild it without restarting glance.
pub static AGGREGATOR: once_cell::sync::Lazy<RwLock<Option<Arc<Aggregator>>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(None));

/// Replace the global aggregator with a freshly built one from current config.
/// Drops the previous instance — for stdio upstreams this kills the old
/// subprocesses (`Child`'s `Drop` sends SIGKILL once we drop the handle).
pub async fn rebuild_from_config() -> Result<()> {
    let cfg = crate::config::load_or_default()?;
    let agg = Aggregator::start(&cfg.upstream_mcps, &cfg.backend.api_key).await;
    let mut guard = AGGREGATOR.write().await;
    *guard = Some(Arc::new(agg));
    Ok(())
}

/// Get a clone of the current aggregator handle (cheap — just an `Arc` bump).
pub async fn current() -> Option<Arc<Aggregator>> {
    AGGREGATOR.read().await.as_ref().cloned()
}

/// Used by the GUI to display + filter upstreams.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamStatusSnapshot {
    pub name: String,
    pub type_label: &'static str,
    pub status: UpstreamStatus,
    pub tool_count: usize,
    pub last_error: Option<String>,
    pub connect_ms: Option<u64>,
    /// Per-client allowlist (empty = expose to all).
    pub clients: Vec<String>,
    /// Whether the *current* glance-mcp process's MCP client is allowed to
    /// see this upstream's tools (resolved against `mcp::current_client()`).
    pub exposed_to_current: bool,
}

/// Run a single upstream connection: handshake + tools/list.
async fn connect_one(spec: UpstreamMcp, fallback_api_key: String) -> UpstreamState {
    let name = spec.name().to_string();
    let type_label = spec.type_label();
    let clients_allowlist = spec.clients().to_vec();
    if !spec.enabled() {
        return UpstreamState {
            name,
            type_label,
            status: RwLock::new(UpstreamStatus::Disabled),
            tools: Vec::new(),
            last_error: RwLock::new(None),
            connect_ms: None,
            clients: clients_allowlist,
            transport: RwLock::new(None),
            spec,
            fallback_api_key,
            consecutive_respawn_failures: AtomicU32::new(0),
            last_failure_at: Mutex::new(None),
        };
    }

    let started = std::time::Instant::now();
    match build_transport(&spec, &fallback_api_key).await {
        Ok((transport, tools)) => {
            let connect_ms = started.elapsed().as_millis() as u64;
            let prefixed = prefix_tools(&name, tools);
            UpstreamState {
                name,
                type_label,
                status: RwLock::new(UpstreamStatus::Connected),
                tools: prefixed,
                last_error: RwLock::new(None),
                connect_ms: Some(connect_ms),
                clients: clients_allowlist,
                transport: RwLock::new(Some(transport)),
                spec,
                fallback_api_key,
                consecutive_respawn_failures: AtomicU32::new(0),
                last_failure_at: Mutex::new(None),
            }
        }
        Err(e) => {
            tracing::warn!("aggregator `{}` start failed: {}", name, e);
            UpstreamState {
                name,
                type_label,
                status: RwLock::new(UpstreamStatus::Failed),
                tools: Vec::new(),
                last_error: RwLock::new(Some(e.to_string())),
                connect_ms: None,
                clients: clients_allowlist,
                transport: RwLock::new(None),
                spec,
                fallback_api_key,
                consecutive_respawn_failures: AtomicU32::new(0),
                last_failure_at: Mutex::new(None),
            }
        }
    }
}

/// Build a transport from the spec: handshake, tools/list, and (if
/// configured) prelude_call. Returns the live transport and the upstream's
/// raw (un-prefixed) tool list. Used by both `connect_one` (initial start)
/// and `Aggregator::respawn_stdio` (self-heal path).
async fn build_transport(
    spec: &UpstreamMcp,
    fallback_api_key: &str,
) -> Result<(Transport, Vec<ToolDefinition>)> {
    let prelude = spec.prelude_call().cloned();
    match spec {
        UpstreamMcp::Stdio {
            name,
            command,
            args,
            env,
            ..
        } => {
            let client = client_stdio::StdioMcpClient::start(command, args, env)
                .await
                .map_err(|e| anyhow::anyhow!("start: {}", e))?;
            let tools = client
                .list_tools()
                .await
                .map_err(|e| anyhow::anyhow!("tools/list: {}", e))?;
            if let Some(p) = &prelude {
                match client.call_tool(&p.tool, p.args.clone()).await {
                    Ok(_) => tracing::info!(
                        upstream = %name,
                        tool = %p.tool,
                        "prelude_call succeeded"
                    ),
                    Err(e) => tracing::warn!(
                        upstream = %name,
                        tool = %p.tool,
                        err = %e,
                        "prelude_call failed (upstream still considered connected)"
                    ),
                }
            }
            Ok((Transport::Stdio(Arc::new(client)), tools))
        }
        UpstreamMcp::StreamableHttp {
            name, url, api_key, ..
        } => {
            let key = if api_key.trim().is_empty() {
                fallback_api_key.to_string()
            } else {
                api_key.clone()
            };
            let client = client_streamable_http::StreamableHttpClient::new(url, &key);
            let tools = client
                .list_tools()
                .await
                .map_err(|e| anyhow::anyhow!("tools/list: {}", e))?;
            if let Some(p) = &prelude {
                match client.call_tool(&p.tool, p.args.clone()).await {
                    Ok(_) => tracing::info!(
                        upstream = %name,
                        tool = %p.tool,
                        "prelude_call succeeded"
                    ),
                    Err(e) => tracing::warn!(
                        upstream = %name,
                        tool = %p.tool,
                        err = %e,
                        "prelude_call failed (upstream still considered connected)"
                    ),
                }
            }
            Ok((Transport::Http(Arc::new(client)), tools))
        }
    }
}

fn prefix_tools(upstream_name: &str, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
    tools
        .into_iter()
        .map(|t| ToolDefinition {
            name: format!("{}{}{}", upstream_name, NAMESPACE_SEP, t.name),
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect()
}

/// Split a namespaced tool name `<upstream>__<remote>` on the first `__`.
/// Returns `None` if there's no separator (caller treats as built-in).
pub fn split_namespaced(tool_name: &str) -> Option<(&str, &str)> {
    let idx = tool_name.find(NAMESPACE_SEP)?;
    let (left, rest) = tool_name.split_at(idx);
    let right = &rest[NAMESPACE_SEP.len()..];
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((left, right))
}

/// Convenience: collapse `Vec<ToolContentBlock>` to a single concatenated text
/// — used by client backends that get content blocks back.
pub fn content_to_string(blocks: &[ToolContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ToolContentBlock::Text { text } => text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Smoke-test a spec without keeping the connection. Used by the GUI's
/// `test_upstream_mcp` command.
pub async fn smoke_test(spec: UpstreamMcp, fallback_api_key: &str) -> SmokeTestResult {
    let started = std::time::Instant::now();
    let name = spec.name().to_string();
    // Forcefully treat as enabled for the test (otherwise it'd report Disabled).
    let enabled_spec = match spec {
        UpstreamMcp::Stdio {
            name,
            command,
            args,
            env,
            clients,
            prelude_call,
            ..
        } => UpstreamMcp::Stdio {
            name,
            command,
            args,
            env,
            enabled: true,
            clients,
            prelude_call,
        },
        UpstreamMcp::StreamableHttp {
            name,
            url,
            api_key,
            clients,
            prelude_call,
            ..
        } => UpstreamMcp::StreamableHttp {
            name,
            url,
            api_key,
            enabled: true,
            clients,
            prelude_call,
        },
    };
    let state = connect_one(enabled_spec, fallback_api_key.to_string()).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let status = *state.status.read().await;
    let last_error = state.last_error.read().await.clone();
    SmokeTestResult {
        name,
        ok: matches!(status, UpstreamStatus::Connected),
        tool_count: state.tools.len(),
        latency_ms,
        error: last_error,
        // sample first 5 tool names (already namespaced) for UI feedback
        sample_tools: state.tools.iter().take(5).map(|t| t.name.clone()).collect(),
    }
    // state drops here — for stdio that kills the subprocess, http is just an Arc<Client> drop
}

#[derive(Debug, Clone, Serialize)]
pub struct SmokeTestResult {
    pub name: String,
    pub ok: bool,
    pub tool_count: usize,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub sample_tools: Vec<String>,
}

/// Templates surfaced by the GUI for one-click installation. Each is a
/// pre-filled spec with placeholder values for user-supplied fields.
#[allow(clippy::vec_init_then_push)]
pub fn list_templates() -> Vec<UpstreamTemplate> {
    use UpstreamMcp::*;
    let mut out: Vec<UpstreamTemplate> = Vec::new();

    out.push(UpstreamTemplate {
        slug: "context7",
        label: "Context7 — library docs lookup",
        description: "Resolves library names to canonical IDs and fetches up-to-date docs. Stdio. Requires API key from context7.com.",
        prompts: vec![PromptField {
            field: "args[1]",
            label: "Context7 API key",
            secret: true,
        }],
        spec: Stdio {
            name: "context7".to_string(),
            command: "context7-mcp".to_string(),
            args: vec!["--api-key".to_string(), "<paste>".to_string()],
            env: HashMap::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "playwright",
        label: "Playwright — browser automation",
        description: "Headless / headed browser via @playwright/mcp@latest. Stdio. No setup needed beyond `npx`.",
        prompts: Vec::new(),
        spec: Stdio {
            name: "playwright".to_string(),
            command: "npx".to_string(),
            args: vec!["@playwright/mcp@latest".to_string()],
            env: HashMap::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "chrome-devtools",
        label: "chrome-devtools — inspect a live Chrome",
        description: "Connects to a running Chrome / Chromium via CDP. Stdio. Requires the chrome-devtools-mcp binary on PATH.",
        prompts: Vec::new(),
        spec: Stdio {
            name: "chrome-devtools".to_string(),
            command: "/usr/local/bin/chrome-devtools-mcp".to_string(),
            args: vec!["--isolated".to_string()],
            env: HashMap::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "mysql",
        label: "MySQL — read-only DB inspection",
        description: "Stdio MCP that wraps a MySQL client. You provide the launcher script.",
        prompts: vec![PromptField {
            field: "command",
            label: "Path to your MySQL MCP launcher",
            secret: false,
        }],
        spec: Stdio {
            name: "mysql".to_string(),
            command: "<your script>".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "zread",
        label: "zread — repo Q&A (GLM-hosted)",
        description:
            "GLM's repo-reading MCP over streamable-HTTP. API key auto-fills from backend.api_key.",
        prompts: Vec::new(),
        spec: StreamableHttp {
            name: "zread".to_string(),
            url: "https://open.bigmodel.cn/api/mcp/zread/mcp".to_string(),
            api_key: String::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "web-search-prime",
        label: "web-search-prime — GLM web search",
        description: "GLM's premium web search over streamable-HTTP. API key auto-fills from backend.api_key.",
        prompts: Vec::new(),
        spec: StreamableHttp {
            name: "web-search-prime".to_string(),
            url: "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp".to_string(),
            api_key: String::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out.push(UpstreamTemplate {
        slug: "web-reader",
        label: "web-reader — fetch + clean a URL",
        description: "GLM's URL → Markdown reader over streamable-HTTP. API key auto-fills from backend.api_key.",
        prompts: Vec::new(),
        spec: StreamableHttp {
            name: "web-reader".to_string(),
            url: "https://open.bigmodel.cn/api/mcp/web_reader/mcp".to_string(),
            api_key: String::new(),
            enabled: true,
            clients: Vec::new(),
            prelude_call: None,
        },
    });

    out
}

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamTemplate {
    pub slug: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Hints for the GUI: which fields the user must fill in before saving.
    pub prompts: Vec<PromptField>,
    pub spec: UpstreamMcp,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptField {
    /// Logical pointer ("args[1]" / "url" / "api_key" / "command") — the GUI
    /// just needs this to highlight the right input.
    pub field: &'static str,
    pub label: &'static str,
    pub secret: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_namespaced_basic() {
        assert_eq!(
            split_namespaced("context7__resolve-library-id"),
            Some(("context7", "resolve-library-id"))
        );
        assert_eq!(
            split_namespaced("playwright__browser_navigate"),
            Some(("playwright", "browser_navigate"))
        );
    }

    #[test]
    fn split_namespaced_no_separator() {
        assert_eq!(split_namespaced("research"), None);
        assert_eq!(split_namespaced("md_read"), None); // single underscore != separator
    }

    #[test]
    fn split_namespaced_edge_cases() {
        // Empty halves are rejected.
        assert_eq!(split_namespaced("__foo"), None);
        assert_eq!(split_namespaced("foo__"), None);
        // Splits on the FIRST `__`, so a remote tool with `__` in its name
        // is preserved (rare but possible).
        assert_eq!(split_namespaced("ctx__a__b"), Some(("ctx", "a__b")));
    }

    #[test]
    fn templates_are_distinct_and_well_formed() {
        let ts = list_templates();
        assert!(ts.len() >= 7);
        let mut seen = std::collections::HashSet::new();
        for t in &ts {
            assert!(seen.insert(t.slug), "duplicate slug {}", t.slug);
            assert!(!t.spec.name().is_empty());
            // Streamable-http templates must have a URL.
            if let UpstreamMcp::StreamableHttp { url, .. } = &t.spec {
                assert!(url.starts_with("https://"));
            }
        }
    }
}
