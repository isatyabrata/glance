// Mirrors glance::config::Config (Rust). Keep in sync with src/config/mod.rs.

export interface RetryConfig {
  max_retries: number;
  base_backoff_ms: number;
  max_backoff_secs: number;
}

export interface BackendConfig {
  base_url: string;
  api_key: string;
  model: string;
  max_tokens: number;
  timeout_secs: number;
  fallback_models: string[];
  retry: RetryConfig;
}

export interface TokensConfig {
  github: string;
}

export interface SubAgentConfig {
  max_iterations: number;
}

export interface ObsidianConfig {
  vault: string;
}

export interface SafetyConfig {
  deny_paths: string[];
  deny_keywords: string[];
}

export type ToolsConfig = Record<string, boolean>;

export interface Config {
  backend: BackendConfig;
  sub_agent: SubAgentConfig;
  tools: ToolsConfig;
  obsidian: ObsidianConfig;
  safety: SafetyConfig;
  events_enabled: boolean;
  tokens: TokensConfig;
}

export interface ToolEntry {
  key: string;
  default_on: boolean;
  category: "read" | "write";
}

export interface BackendCheck {
  ok: boolean;
  status: number;
  model: string;
  latency_ms: number;
  error: string | null;
}

export interface PingResult {
  ok: boolean;
  status: number;
  latency_ms: number;
  error: string | null;
}

export interface GitHubTokenCheck {
  ok: boolean;
  status: number;
  login: string | null;
  scopes: string[];
  latency_ms: number;
  error: string | null;
}

export interface EventLine {
  ts: string;
  tool: string;
  duration_ms: number;
  ok: boolean;
  bytes_in: number;
  bytes_out: number;
  savings_pct: number;
  /** Total GLM tokens (prompt + completion) consumed by the tool call's
   *  internal sub-agent loops. 0 for tools without a sub-agent and for
   *  legacy rows recorded before tracking existed. */
  glm_tokens?: number;
  glm_prompt_tokens?: number;
  glm_completion_tokens?: number;
  iters?: number;
  error?: string | null;
}

export interface TodayStats {
  calls: number;
  bytes_in: number;
  bytes_out: number;
  savings_pct: number;
  ok_count: number;
  err_count: number;
  /** Sum of GLM `tokens` across every event recorded today. */
  glm_total_tokens: number;
  /** glm_total_tokens / glm_billed_calls (calls with tokens>0). */
  glm_avg_per_call: number;
  /** How many of today's calls actually drove the GLM backend. */
  glm_billed_calls: number;
}

// ── Upstream MCP aggregator ─────────────────────────────────────────────────
//
// Mirrors `glance::config::UpstreamMcp` (Rust). The discriminator is `type`
// with snake_case values "stdio" / "streamable_http".

export type McpClientId = "claude" | "codex" | "cursor";

export interface UpstreamMcpStdio {
  type: "stdio";
  name: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  enabled: boolean;
  /** Per-client allowlist. Empty = exposed to every client. */
  clients: McpClientId[];
}

export interface UpstreamMcpStreamableHttp {
  type: "streamable_http";
  name: string;
  url: string;
  api_key: string;
  enabled: boolean;
  clients: McpClientId[];
}

export type UpstreamMcp = UpstreamMcpStdio | UpstreamMcpStreamableHttp;

export type UpstreamStatus = "connected" | "failed" | "disabled";

export interface UpstreamStatusSnapshot {
  name: string;
  type_label: "stdio" | "streamable_http";
  status: UpstreamStatus;
  tool_count: number;
  last_error: string | null;
  connect_ms: number | null;
  clients: McpClientId[];
  /** Whether the current MCP client (the glance-mcp process talking to one
   *  of claude/codex/cursor) sees this upstream's tools. */
  exposed_to_current: boolean;
}

export interface UpstreamMcpListEntry {
  spec: UpstreamMcp;
  runtime: UpstreamStatusSnapshot | null;
}

export interface SmokeTestResult {
  name: string;
  ok: boolean;
  tool_count: number;
  latency_ms: number;
  error: string | null;
  sample_tools: string[];
}

export interface UpstreamPromptField {
  field: string;
  label: string;
  secret: boolean;
}

export interface UpstreamTemplate {
  slug: string;
  label: string;
  description: string;
  prompts: UpstreamPromptField[];
  spec: UpstreamMcp;
}

// ── rtk (rtk-ai/rtk) ────────────────────────────────────────────────────────
//
// Mirrors `glance_app::commands::Rtk*` structs (Rust). rtk's stats live in
// `~/Library/Application Support/rtk/history.db`; the Tauri commands shell
// out to the rtk CLI for status / install / summary, and to sqlite3 for
// per-command history rows.

export type RtkClient = "claude" | "codex" | "cursor";

export interface RtkStatus {
  installed: boolean;
  binary_path: string | null;
  version: string | null;
  claude_hook: boolean;
  codex_agents_md: boolean;
  cursor_hook: boolean;
}

export interface RtkGain {
  total_commands: number;
  total_input: number;
  total_output: number;
  total_saved: number;
  avg_savings_pct: number;
  total_time_ms: number;
  avg_time_ms: number;
}

export interface RtkHistoryEntry {
  timestamp: string;
  command: string;
  input_bytes: number;
  output_bytes: number;
  savings_pct: number;
  time_ms: number;
}

export interface RtkUpdateCheck {
  current: string | null;
  latest: string | null;
  outdated: boolean;
  source: string;
  error: string | null;
}

export interface RtkUpdateResult {
  ok: boolean;
  method: string;
  stdout: string;
  stderr: string;
}

// ── 08 ccusage (ryoppippi/ccusage) ──────────────────────────────────────────
//
// Mirrors `glance_app::commands::Ccusage*` (Rust). The Tauri commands shell
// out to `npx -y ccusage@latest <subcmd> --json`, so the structures here
// match ccusage's documented JSON shape with one addition: a derived
// `source` tag ("claude" / "codex" / "mixed" / "none") computed from the
// model names ccusage attaches to each row.

export type CcusageSource = "claude" | "codex" | "mixed" | "none";

export interface CcusageStatus {
  installed: boolean;
  version: string | null;
  claude_jsonl_count: number;
  codex_jsonl_count: number;
  error: string | null;
}

export interface CcusageDailyEntry {
  date: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  total_tokens: number;
  estimated_cost_usd: number;
  models_used: string[];
  source: CcusageSource;
}

export interface CcusageDailyResponse {
  entries: CcusageDailyEntry[];
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_creation_tokens: number;
  total_cache_read_tokens: number;
  total_tokens: number;
  total_cost_usd: number;
}

export interface CcusageSessionEntry {
  session_id: string;
  last_activity: string;
  project: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  total_tokens: number;
  estimated_cost_usd: number;
  models_used: string[];
  source: CcusageSource;
}
