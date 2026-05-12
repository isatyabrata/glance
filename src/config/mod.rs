//! Config schema + loader.
//!
//! Resolution order:
//! 1. `~/.glance/config.toml` (user-level)
//! 2. `./glance.toml` (project-level, overrides user)
//! 3. env vars (`GLANCE_API_KEY`, `GLANCE_BASE_URL`, ...) — highest priority
//!
//! Missing fields fall back to [`Config::default`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub sub_agent: SubAgentConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub obsidian: ObsidianConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    /// When true, every `tools/call` invocation appends a JSON line to
    /// `~/.glance/events.jsonl`. Used by the GUI Logs tab. Default: false.
    #[serde(default)]
    pub events_enabled: bool,
    /// Third-party API tokens — glance acts as the central manager so
    /// callers don't have to juggle env vars across shells / IDE sessions.
    /// Env vars (GITHUB_TOKEN, etc) still win when set — see `tokens()`
    /// helpers below.
    #[serde(default)]
    pub tokens: TokensConfig,
    /// Upstream MCP servers that glance proxies through. Each entry is
    /// connected on startup and its tools merged into glance's `tools/list`
    /// under a `<name>__` namespace prefix. See [`UpstreamMcp`].
    #[serde(default)]
    pub upstream_mcps: Vec<UpstreamMcp>,
    /// Per-tool client allowlist for built-in tools. Map key is the tool
    /// name (e.g. `"chrome"`), value is the list of normalized client ids
    /// allowed to see/call it. Missing key OR empty list = exposed to all
    /// clients (back-compat). Mirrors `UpstreamMcp.clients` semantics.
    #[serde(default)]
    pub tools_clients: std::collections::HashMap<String, Vec<String>>,
}

fn default_true() -> bool {
    true
}

/// One-shot tool call fired by the aggregator after the upstream completes
/// its `initialize` handshake. Used for MCPs that require an explicit
/// `connect_*` / `auth_*` step before any other tool works (notably
/// `mcp-mysql-server` which won't auto-read env vars; you must call
/// `connect_db { url: "mysql://..." }` first or every other tool fails).
///
/// Failures are logged but don't take the upstream offline — the caller's
/// next tool call will surface the real error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreludeCall {
    /// Tool name to call (NOT namespaced — this is the upstream's local
    /// tool, e.g. `connect_db`).
    pub tool: String,
    /// Arguments object. Defaults to `{}` if omitted.
    #[serde(default = "default_empty_obj")]
    pub args: serde_json::Value,
}

fn default_empty_obj() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Connection spec for an upstream MCP server we proxy through glance.
///
/// Two transports are supported:
/// - `stdio`: spawn a subprocess and speak JSON-RPC over stdin/stdout
///   (e.g. `npx @playwright/mcp@latest`).
/// - `streamable_http`: POST JSON-RPC to a remote URL and read SSE back
///   (e.g. GLM's `https://open.bigmodel.cn/api/mcp/zread/mcp`).
///
/// The `name` is the namespacing prefix used in `tools/list` — e.g. an
/// upstream named `context7` exposes tools as `context7__resolve-library-id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamMcp {
    Stdio {
        /// Unique short identifier — used for tool namespacing. Must match
        /// `^[a-z0-9_-]+$` in practice (we don't enforce, but `__` separator
        /// requires it not contain `__`).
        name: String,
        /// Executable to launch. Resolved against PATH.
        command: String,
        /// CLI arguments passed to the subprocess.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables for the subprocess (in addition to
        /// inherited env).
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        /// When false, the upstream is configured but not connected on
        /// startup (skip silently). Default true.
        #[serde(default = "default_true")]
        enabled: bool,
        /// Per-client allowlist. Empty = exposed to every client. Non-empty
        /// = only the named clients see this upstream's tools (case-insensitive
        /// match against `claude` / `codex` / `cursor`). Used e.g. to keep
        /// `chrome-devtools` from codex (which has its own chrome plugin).
        #[serde(default)]
        clients: Vec<String>,
        /// One-shot tool call fired right after `initialize` (e.g. `connect_db`
        /// for `mcp-mysql-server`). See [`PreludeCall`].
        #[serde(default)]
        prelude_call: Option<PreludeCall>,
    },
    StreamableHttp {
        name: String,
        url: String,
        /// Bearer token sent as `Authorization: Bearer ...`. If empty, falls
        /// back to `backend.api_key` (so GLM-hosted MCPs Just Work).
        #[serde(default)]
        api_key: String,
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        clients: Vec<String>,
        #[serde(default)]
        prelude_call: Option<PreludeCall>,
    },
}

impl UpstreamMcp {
    pub fn name(&self) -> &str {
        match self {
            UpstreamMcp::Stdio { name, .. } => name,
            UpstreamMcp::StreamableHttp { name, .. } => name,
        }
    }
    pub fn enabled(&self) -> bool {
        match self {
            UpstreamMcp::Stdio { enabled, .. } => *enabled,
            UpstreamMcp::StreamableHttp { enabled, .. } => *enabled,
        }
    }
    pub fn type_label(&self) -> &'static str {
        match self {
            UpstreamMcp::Stdio { .. } => "stdio",
            UpstreamMcp::StreamableHttp { .. } => "streamable_http",
        }
    }
    pub fn clients(&self) -> &[String] {
        match self {
            UpstreamMcp::Stdio { clients, .. } => clients,
            UpstreamMcp::StreamableHttp { clients, .. } => clients,
        }
    }
    pub fn prelude_call(&self) -> Option<&PreludeCall> {
        match self {
            UpstreamMcp::Stdio { prelude_call, .. } => prelude_call.as_ref(),
            UpstreamMcp::StreamableHttp { prelude_call, .. } => prelude_call.as_ref(),
        }
    }
    /// `true` if this upstream is exposed to the given normalized client
    /// id (`claude` / `codex` / `cursor` / `unknown`). Empty allowlist =
    /// exposed everywhere.
    pub fn allowed_for(&self, client: &str) -> bool {
        let list = self.clients();
        if list.is_empty() {
            return true;
        }
        list.iter().any(|c| c.eq_ignore_ascii_case(client))
    }
}

/// Credentials for non-LLM APIs glance speaks to. Each field is optional;
/// `tokens.github` covers `repo_explore` GitHub calls. Future: gitlab,
/// linear, etc.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokensConfig {
    /// GitHub personal access token (fine-grained or classic). Used by
    /// `repo_explore` for code search + higher rate limits. Resolution
    /// order: `GITHUB_TOKEN` env var > this field. Empty = no token used.
    #[serde(default)]
    pub github: String,
}

impl TokensConfig {
    /// Resolved GitHub token: env var first, then config field. Returns
    /// `None` if both are empty / unset.
    pub fn resolved_github(&self) -> Option<String> {
        if let Ok(v) = std::env::var("GITHUB_TOKEN") {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
        let t = self.github.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u32,
    /// Fallback models to try if `model` keeps returning 429 / 5xx after
    /// exhausting `retry.max_retries`. Each entry is tried in order with the
    /// same retry budget. Empty (default) = no fallback, caller sees the
    /// final error after retries on `model` alone.
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            api_key: String::new(),
            model: "glm-5.1".to_string(),
            max_tokens: default_max_tokens(),
            timeout_secs: default_timeout(),
            fallback_models: Vec::new(),
            retry: RetryConfig::default(),
        }
    }
}

fn default_max_tokens() -> u32 {
    8000
}
fn default_timeout() -> u32 {
    // 90 s per HTTP request. Reasoning models (deepseek-v4-pro) can spend
    // 40-80 s generating hidden thinking tokens before the visible response.
    // 90 s gives them room while still triggering the fallback path if
    // they're truly stuck.
    90
}

/// Retry policy for backend HTTP calls (chat completions + vision).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Attempts per model before falling back / giving up. 0 = no retries
    /// (one shot only). Default 3.
    #[serde(default = "default_retry_max")]
    pub max_retries: u32,
    /// Base exponential-backoff in milliseconds. Wait between attempt N and
    /// N+1 is `base_backoff_ms * 3^N` (so 1000ms → 1s, 3s, 9s by default).
    /// Default 1000.
    #[serde(default = "default_retry_base_ms")]
    pub base_backoff_ms: u64,
    /// Cap on `Retry-After` header in seconds. Servers can ask for absurd
    /// waits; we honor up to this. Default 30.
    #[serde(default = "default_retry_max_secs")]
    pub max_backoff_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_retry_max(),
            base_backoff_ms: default_retry_base_ms(),
            max_backoff_secs: default_retry_max_secs(),
        }
    }
}

fn default_retry_max() -> u32 {
    3
}
fn default_retry_base_ms() -> u64 {
    1000
}
fn default_retry_max_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentConfig {
    #[serde(default = "default_iterations")]
    pub max_iterations: u32,
    /// Wall-clock budget for one sub-agent run. The default (175 s) leaves a
    /// 5 s safety margin under the MCP client's tools/call timeout (180 s for
    /// codex, 120 s for claude/cursor). When the deadline hits, sub_agent
    /// returns the partial summary it has.
    #[serde(default = "default_deadline_secs")]
    pub deadline_secs: u64,
    /// Per-chat-call timeout. One stuck GLM response shouldn't eat the whole
    /// budget — abort that call and let the loop continue with what it has.
    #[serde(default = "default_chat_timeout_secs")]
    pub chat_timeout_secs: u64,
}

impl Default for SubAgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_iterations(),
            deadline_secs: default_deadline_secs(),
            chat_timeout_secs: default_chat_timeout_secs(),
        }
    }
}

fn default_iterations() -> u32 {
    8
}

fn default_deadline_secs() -> u64 {
    // Total sub-agent budget. Aligned with the transport layer's 175 s
    // guard. With a 90 s HTTP timeout, reasoning models can fit 1-2 slow
    // calls + 2-3 fast fallback calls before the deadline fires.
    175
}

fn default_chat_timeout_secs() -> u64 {
    // Per-iteration timeout. Must be > HTTP timeout (90 s) so individual
    // slow calls complete rather than being cut off. With 170 s, a call
    // that takes the full 90 s HTTP budget still has room for the fallback
    // model to respond. Stays under the 175 s transport guard.
    170
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    // Read-only — default ON
    pub research: bool,
    pub explain: bool,
    pub search: bool,
    pub md_read: bool,
    pub md_outline: bool,
    pub obsidian_read: bool,
    pub obsidian_search: bool,
    pub obsidian_backlinks: bool,
    // Write/patch — default OFF
    pub write_tests: bool,
    pub write_docs: bool,
    pub fix_lint: bool,
    pub md_write: bool,
    pub obsidian_write: bool,
    // Gateway tools — default ON (read-only, low-risk).
    // `web_fetch` and `repo_explore` are pure local / direct-API (no GLM quota).
    // `image_describe` and `web_search` route to GLM so callers (Claude/Codex)
    // don't burn Anthropic vision tokens or built-in WebSearch quota.
    pub web_fetch: bool,
    pub repo_explore: bool,
    pub image_describe: bool,
    pub web_search: bool,
    /// Drive the user's live Chrome via the Glance Chrome bridge extension.
    /// Off by default — needs `glance chrome install` + loading the extension.
    pub chrome: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            research: true,
            explain: true,
            search: true,
            md_read: true,
            md_outline: true,
            obsidian_read: true,
            obsidian_search: true,
            obsidian_backlinks: true,
            write_tests: false,
            write_docs: false,
            fix_lint: false,
            md_write: false,
            obsidian_write: false,
            web_fetch: true,
            repo_explore: true,
            image_describe: true,
            web_search: true,
            chrome: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObsidianConfig {
    /// User-set vault path. Empty → fall back to project AGENTS.md/CLAUDE.md
    /// declaration, then to the hardcoded iCloud default.
    #[serde(default)]
    pub vault: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "default_deny_paths")]
    pub deny_paths: Vec<String>,
    #[serde(default = "default_deny_keywords")]
    pub deny_keywords: Vec<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            deny_paths: default_deny_paths(),
            deny_keywords: default_deny_keywords(),
        }
    }
}

fn default_deny_paths() -> Vec<String> {
    [
        "auth",
        "oauth",
        "jwt",
        "session",
        "security",
        "permission",
        "rbac",
        "payment",
        "billing",
        "invoice",
        "migration",
        "schema",
        "infra",
        "terraform",
        ".github/workflows",
        ".env",
        "secret",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_deny_keywords() -> Vec<String> {
    ["production", "deploy", "encrypt", "decrypt"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// User-level config path: `~/.glance/config.toml`.
pub fn user_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("config.toml"))
}

/// Project-level override: `./glance.toml` in cwd.
pub fn project_config_path() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join("glance.toml"))
}

/// Load the merged config from disk. Missing files are not an error — defaults
/// + env vars cover the gap.
pub fn load_or_default() -> Result<Config> {
    let mut cfg = Config::default();

    if let Ok(path) = user_config_path() {
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            cfg = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        }
    }

    if let Some(path) = project_config_path() {
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            // Project file fully overrides user file (don't bother with deep
            // merge — small surface area).
            cfg = toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        }
    }

    // Env vars
    if let Ok(v) = std::env::var("GLANCE_API_KEY") {
        cfg.backend.api_key = v;
    }
    if let Ok(v) = std::env::var("GLANCE_BASE_URL") {
        cfg.backend.base_url = v;
    }
    if let Ok(v) = std::env::var("GLANCE_MODEL") {
        cfg.backend.model = v;
    }

    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_mcp_stdio_round_trip() {
        let raw = r#"
[[upstream_mcps]]
type = "stdio"
name = "playwright"
command = "npx"
args = ["@playwright/mcp@latest"]
enabled = true

[upstream_mcps.env]
NODE_ENV = "production"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.upstream_mcps.len(), 1);
        match &cfg.upstream_mcps[0] {
            UpstreamMcp::Stdio {
                name,
                command,
                args,
                env,
                enabled,
                clients: _,
                prelude_call: _,
            } => {
                assert_eq!(name, "playwright");
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["@playwright/mcp@latest".to_string()]);
                assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("production"));
                assert!(*enabled);
            }
            _ => panic!("expected Stdio variant"),
        }
        // Round-trip: serialize → reparse, verify identity on the variant.
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg2.upstream_mcps.len(), 1);
        assert_eq!(cfg2.upstream_mcps[0].name(), "playwright");
        assert_eq!(cfg2.upstream_mcps[0].type_label(), "stdio");
    }

    #[test]
    fn upstream_mcp_streamable_http_round_trip() {
        let raw = r#"
[[upstream_mcps]]
type = "streamable_http"
name = "zread"
url = "https://open.bigmodel.cn/api/mcp/zread/mcp"
api_key = "secret-token"
enabled = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.upstream_mcps.len(), 1);
        match &cfg.upstream_mcps[0] {
            UpstreamMcp::StreamableHttp {
                name,
                url,
                api_key,
                enabled,
                clients: _,
                prelude_call: _,
            } => {
                assert_eq!(name, "zread");
                assert_eq!(url, "https://open.bigmodel.cn/api/mcp/zread/mcp");
                assert_eq!(api_key, "secret-token");
                assert!(!*enabled);
            }
            _ => panic!("expected StreamableHttp variant"),
        }
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg2.upstream_mcps[0].type_label(), "streamable_http");
        assert!(!cfg2.upstream_mcps[0].enabled());
    }

    #[test]
    fn upstream_mcp_defaults() {
        // Minimal stdio entry should default enabled=true, args=empty, env=empty.
        let raw = r#"
[[upstream_mcps]]
type = "stdio"
name = "ctx7"
command = "context7-mcp"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        match &cfg.upstream_mcps[0] {
            UpstreamMcp::Stdio {
                args, env, enabled, ..
            } => {
                assert!(args.is_empty());
                assert!(env.is_empty());
                assert!(*enabled);
            }
            _ => panic!(),
        }
    }
}
