//! Model Context Protocol implementation (server side).
//!
//! Spec: <https://modelcontextprotocol.io/>
//!
//! Currently only the stdio transport is supported. The HTTP transport may be
//! added later for IDE integrations that don't spawn a subprocess.

pub mod outline;
pub mod protocol;
pub mod sub_agent;
pub mod transport;

use std::sync::OnceLock;

/// MCP client identity captured during the `initialize` handshake. One
/// glance-mcp process serves exactly one client (Claude Code spawns its own
/// subprocess, codex spawns its own, cursor spawns its own), so this is set
/// once per process.
static CURRENT_CLIENT: OnceLock<String> = OnceLock::new();

/// Normalize the raw `clientInfo.name` from MCP initialize into one of:
/// `"claude"` / `"codex"` / `"cursor"` / `"unknown"`. Uses lowercased
/// substring containment so future client name variants ("claude-code",
/// "claude-desktop", "codex-cli") all classify cleanly.
pub fn normalize_client(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("claude") {
        return "claude".into();
    }
    if lower.contains("codex") {
        return "codex".into();
    }
    if lower.contains("cursor") {
        return "cursor".into();
    }
    "unknown".into()
}

/// Record the client identity. Called from `transport::dispatch` on the
/// `initialize` request. Safe to call repeatedly; the first wins.
pub fn record_client(name: &str) {
    let _ = CURRENT_CLIENT.set(normalize_client(name));
}

/// Return the current client identity, or `"unknown"` if `initialize` has
/// not been received yet (e.g. during startup before the first request).
pub fn current_client() -> &'static str {
    CURRENT_CLIENT
        .get()
        .map(String::as_str)
        .unwrap_or("unknown")
}
