//! Optional event emitter for the GUI Logs tab and the `glance stats` CLI.
//!
//! When `events_enabled = true` (config flag, default false), each tool invocation
//! appends one JSON line to `~/.glance/events.jsonl`. The Tauri GUI tails this
//! file via the `notify` crate and renders it in the Logs tab; `glance stats`
//! aggregates it into a per-tool savings table.
//!
//! The MCP server itself doesn't depend on this — if writing fails, we just log
//! a warning and continue.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// One event line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEvent {
    /// ISO-8601 UTC timestamp.
    pub ts: String,
    /// Tool name (e.g. "research", "md_read").
    pub tool: String,
    /// Short summary of args (first ~200 chars of the JSON).
    pub args_summary: String,
    /// Sub-agent iterations consumed (cumulative across every `sub_agent::run`
    /// call made by this MCP invocation).
    pub iters: u32,
    /// Total GLM tokens (prompt + completion) consumed by this invocation's
    /// internal sub-agent loops. This is the cost paid in the cheap pool to
    /// produce the savings reported in `bytes_in` / `bytes_out`.
    pub tokens: u32,
    /// GLM prompt tokens (subset of `tokens`). Optional — older lines may
    /// omit this field.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub glm_prompt_tokens: u32,
    /// GLM completion tokens (subset of `tokens`).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub glm_completion_tokens: u32,
    /// Prompt tokens that the backend served from its cache (subset of
    /// `glm_prompt_tokens`). Empty / 0 when the backend doesn't report it.
    /// Cache-hit-rate = `glm_cached_tokens / glm_prompt_tokens`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub glm_cached_tokens: u32,
    /// Anthropic-only: prompt tokens charged at the cache-WRITE rate (1.25×).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub glm_cache_creation_tokens: u32,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Optional error message when `ok=false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bytes the sub-agent's internal tool calls returned to it (cumulative
    /// across the whole MCP call). This is the "raw file content the calling
    /// LLM never had to see" volume.
    #[serde(default)]
    pub bytes_in: u64,
    /// Bytes returned to the MCP caller (final text content blocks).
    #[serde(default)]
    pub bytes_out: u64,
    /// Estimated tokens the caller would have spent doing the file reads
    /// itself, minus the tokens the summary cost. Rough char→token ratio of 4.
    /// Saturating subtraction so a verbose answer can't go negative.
    #[serde(default)]
    pub estimated_caller_savings_tokens: u64,
}

/// Default event log location.
pub fn events_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("events.jsonl"))
}

/// Append a single event line. Best-effort; logs and swallows IO errors.
pub fn append(event: &ToolEvent) {
    if let Err(e) = try_append(event) {
        tracing::warn!("events: failed to append event: {}", e);
    }
}

fn try_append(event: &ToolEvent) -> Result<()> {
    let path = events_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let line = serde_json::to_string(event)?;
    writeln!(f, "{}", line)?;
    Ok(())
}

/// Per-call byte counters captured by the transport layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByteAccounting {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Per-MCP-call mutable accounting. The transport layer creates one of these
/// per `tools/call`, sets it as a task-local, and any sub-agent loop running
/// inside that task increments counters on it. Avoids threading a context
/// argument through every tool function.
#[derive(Debug, Default)]
pub struct CallCtx {
    /// Bytes pulled into the sub-agent loop from internal tool calls
    /// (`read_file`/`grep`/`list_dir` results) — the volume of file content
    /// the calling LLM never had to see.
    pub bytes_in: std::sync::atomic::AtomicU64,
    /// GLM prompt tokens summed across every `sub_agent::run` invocation
    /// made under this CallCtx scope.
    pub glm_prompt_tokens: std::sync::atomic::AtomicU64,
    /// GLM completion tokens summed across the same invocations.
    pub glm_completion_tokens: std::sync::atomic::AtomicU64,
    /// Of the prompt tokens above, how many were served from the backend's
    /// prefix cache (cache-read, billed at ~0.1×).
    pub glm_cached_tokens: std::sync::atomic::AtomicU64,
    /// Anthropic-only: prompt tokens charged at the cache-WRITE rate (1.25×).
    pub glm_cache_creation_tokens: std::sync::atomic::AtomicU64,
    /// Sub-agent iterations consumed across the same invocations.
    pub glm_iterations: std::sync::atomic::AtomicU32,
}

impl CallCtx {
    pub fn add_bytes_in(&self, n: u64) {
        self.bytes_in
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn snapshot_in(&self) -> u64 {
        self.bytes_in.load(std::sync::atomic::Ordering::Relaxed)
    }
    /// Add the prompt+completion token counts from one chat-completion turn,
    /// plus the cache-read / cache-write breakdown if the backend reports it.
    pub fn add_glm(&self, prompt: u32, completion: u32, cached: u32, cache_creation: u32) {
        self.glm_prompt_tokens
            .fetch_add(prompt as u64, std::sync::atomic::Ordering::Relaxed);
        self.glm_completion_tokens
            .fetch_add(completion as u64, std::sync::atomic::Ordering::Relaxed);
        self.glm_cached_tokens
            .fetch_add(cached as u64, std::sync::atomic::Ordering::Relaxed);
        self.glm_cache_creation_tokens
            .fetch_add(cache_creation as u64, std::sync::atomic::Ordering::Relaxed);
    }
    /// Increment the per-call sub-agent iteration counter by one.
    pub fn add_iter(&self) {
        self.glm_iterations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    /// `(prompt, completion, iterations)` so far.
    pub fn snapshot_glm(&self) -> (u64, u64, u32) {
        (
            self.glm_prompt_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            self.glm_completion_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            self.glm_iterations
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    /// `(cached, cache_creation)` totals — populated only when the backend
    /// returns cache metrics. Both are subsets of `glm_prompt_tokens`.
    pub fn snapshot_cache(&self) -> (u64, u64) {
        (
            self.glm_cached_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
            self.glm_cache_creation_tokens
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

tokio::task_local! {
    pub static CALL_CTX: std::sync::Arc<CallCtx>;
}

/// Helper: from inside a tool, add to the current call's `bytes_in`. No-op
/// when not running under a CALL_CTX scope (e.g. unit tests).
pub fn add_bytes_in(n: u64) {
    let _ = CALL_CTX.try_with(|ctx| ctx.add_bytes_in(n));
}

/// Helper: from inside `sub_agent::run`, fold one chat turn's token usage
/// back into the current call's CallCtx. No-op outside a CALL_CTX scope.
pub fn add_glm_tokens(prompt: u32, completion: u32, cached: u32, cache_creation: u32) {
    let _ = CALL_CTX.try_with(|ctx| ctx.add_glm(prompt, completion, cached, cache_creation));
}

/// Helper: from inside `sub_agent::run`, count one iteration of the loop.
pub fn add_glm_iter() {
    let _ = CALL_CTX.try_with(|ctx| ctx.add_iter());
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Per-call GLM token accounting captured by the transport layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenAccounting {
    pub prompt: u32,
    pub completion: u32,
    pub iters: u32,
    /// Prompt tokens served from the backend's prefix cache (subset of `prompt`).
    pub cached: u32,
    /// Anthropic-only: tokens billed at cache-WRITE rate.
    pub cache_creation: u32,
}

impl TokenAccounting {
    pub fn total(&self) -> u32 {
        self.prompt.saturating_add(self.completion)
    }
}

/// Build a `ToolEvent` and append, when the `events_enabled` flag is set.
#[allow(clippy::too_many_arguments)]
pub fn record(
    enabled: bool,
    tool: &str,
    args: &serde_json::Value,
    tokens: TokenAccounting,
    duration_ms: u64,
    ok: bool,
    error: Option<String>,
    bytes: ByteAccounting,
) {
    if !enabled {
        return;
    }
    let args_summary = {
        let s = args.to_string();
        if s.chars().count() > 200 {
            let head: String = s.chars().take(200).collect();
            format!("{}…", head)
        } else {
            s
        }
    };
    let savings = estimate_savings_tokens(bytes.bytes_in, bytes.bytes_out);
    let event = ToolEvent {
        ts: Utc::now().to_rfc3339(),
        tool: tool.to_string(),
        args_summary,
        iters: tokens.iters,
        tokens: tokens.total(),
        glm_prompt_tokens: tokens.prompt,
        glm_completion_tokens: tokens.completion,
        glm_cached_tokens: tokens.cached,
        glm_cache_creation_tokens: tokens.cache_creation,
        duration_ms,
        ok,
        error,
        bytes_in: bytes.bytes_in,
        bytes_out: bytes.bytes_out,
        estimated_caller_savings_tokens: savings,
    };
    append(&event);
}

/// Rough char→token estimate at 4 chars/token. Saturating so a verbose answer
/// never produces a negative "savings" number.
pub fn estimate_savings_tokens(bytes_in: u64, bytes_out: u64) -> u64 {
    (bytes_in / 4).saturating_sub(bytes_out / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callctx_add_glm_accumulates_across_turns() {
        // Simulates a sub-agent run that goes through three chat turns. The
        // CallCtx should fold every (prompt, completion) pair into the
        // running totals so the transport layer reads back the cumulative
        // cost — not just the last turn.
        let ctx = CallCtx::default();
        ctx.add_glm(1_000, 200, 800, 0);
        ctx.add_iter();
        ctx.add_glm(500, 50, 400, 0);
        ctx.add_iter();
        ctx.add_glm(2_000, 300, 1_900, 0);
        ctx.add_iter();

        let (prompt, completion, iters) = ctx.snapshot_glm();
        assert_eq!(prompt, 3_500);
        assert_eq!(completion, 550);
        assert_eq!(iters, 3);
        let (cached, _creation) = ctx.snapshot_cache();
        assert_eq!(cached, 3_100);
    }

    #[test]
    fn callctx_snapshot_default_is_zero() {
        // A freshly-built CallCtx — the case for tools that don't run a
        // sub-agent at all (e.g. md_outline). Snapshot must be all zeros so
        // the recorded event line shows tokens=0 rather than garbage.
        let ctx = CallCtx::default();
        assert_eq!(ctx.snapshot_in(), 0);
        assert_eq!(ctx.snapshot_glm(), (0, 0, 0));
    }

    #[test]
    fn token_accounting_total_sums_prompt_and_completion() {
        let t = TokenAccounting {
            prompt: 1_000,
            completion: 250,
            iters: 4,
            cached: 0,
            cache_creation: 0,
        };
        assert_eq!(t.total(), 1_250);
    }
}
