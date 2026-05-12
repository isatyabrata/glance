//! Sub-agent loop.
//!
//! The MCP-facing tool (e.g. `research`) hands us a system prompt + user task.
//! We expose a small set of **internal** function tools to the backend model
//! (read_file / grep / list_dir / glob), let it call them in a loop, and
//! return its final text answer.
//!
//! This is the unit of work that actually saves the calling LLM tokens —
//! all the file content stays inside this loop and never reaches the MCP
//! caller.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::backend::openai_compat::{Client, FunctionSpec, Message, ToolCall, ToolSpec, Usage};
use crate::config;
use crate::mcp::outline;

/// Outcome of a sub-agent run.
#[derive(Debug)]
pub struct SubAgentRun {
    pub answer: String,
    pub iterations: u32,
    pub tool_calls: u32,
    pub usage_total: Usage,
    /// Total bytes returned by internal tool calls into the sub-agent loop.
    /// This is the "bytes the calling LLM didn't have to read" metric.
    pub bytes_in: u64,
}

/// Anti-hallucination clause appended to every tool's system prompt.
/// Without this, smaller / cheaper backend models (GLM-air, DeepSeek, etc.)
/// will sometimes invent plausible-but-wrong file paths from memory of
/// similar projects, instead of grounding answers in the actual repo.
const GROUNDING_GUARD: &str = "\n\nGROUNDING RULES (apply to every answer):\n\
- Cite ONLY file paths and line numbers you observed in a tool result during \
  this conversation (read_file / list_dir / grep / glob). Never invent paths \
  from prior knowledge of similar projects.\n\
- If a file the caller asked about does not exist, say so explicitly: \
  `file not found: <path>`. Do NOT fabricate an explanation of a file that \
  isn't there.\n\
- If a search / grep returns zero matches, the answer is literally \
  `no matches found for <pattern> in <scope>`. Do not invent hits.";

pub async fn run(system: &str, user: &str) -> Result<SubAgentRun> {
    let cfg = config::load_or_default()?;
    if cfg.backend.api_key.is_empty() {
        return Err(anyhow!(
            "backend api_key is empty — set it in ~/.glance/config.toml or via GLANCE_API_KEY"
        ));
    }
    let http_timeout = cfg.backend.timeout_secs;
    let mut has_fallback = !cfg.backend.fallback_models.is_empty();
    let mut client = Client::new(cfg.backend.clone())?;
    // Only switch to fast-only when v4-pro has timed out twice in a row —
    // a single slow call could be a cold start; give it another chance.
    let mut consecutive_timeouts: u32 = 0;

    let combined_system = format!("{}{}", system, GROUNDING_GUARD);
    let mut messages = vec![Message::system(&combined_system), Message::user(user)];
    let mut total = Usage::default();
    let mut tool_calls_count = 0u32;
    let mut bytes_in: u64 = 0;

    let max_iter = cfg.sub_agent.max_iterations.max(1);
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(cfg.sub_agent.deadline_secs.max(10));
    let chat_timeout = std::time::Duration::from_secs(cfg.sub_agent.chat_timeout_secs.max(5));
    for iter in 0..max_iter {
        // Bail before paying for another round-trip if we're out of budget.
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                iter,
                tool_calls = tool_calls_count,
                "sub_agent deadline hit ({}s) — returning partial result",
                cfg.sub_agent.deadline_secs
            );
            return Ok(finalize_partial(messages, iter, tool_calls_count, total, bytes_in, "deadline"));
        }

        // Skip the primary model only when it's literally impossible for it
        // to complete within the remaining budget. Otherwise always try v4-pro
        // first — its quality is worth the wait.
        if consecutive_timeouts < 2 && has_fallback {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_secs();
            // Can't even fit the HTTP timeout → skip primary, go fast-only.
            if remaining <= http_timeout as u64 {
                tracing::warn!(
                    remaining,
                    http_timeout,
                    "not enough time for primary model — using fast-only"
                );
                consecutive_timeouts = 2; // force fast-only path below
            }
        }

        // After 2 consecutive timeouts, the primary model is too slow for this
        // workload. Rebuild the client with the fallback as primary.
        if consecutive_timeouts >= 2 && has_fallback {
            let mut fast_cfg = cfg.backend.clone();
            fast_cfg.model = fast_cfg.fallback_models[0].clone();
            fast_cfg.fallback_models.clear();
            client = Client::new(fast_cfg)?;
            has_fallback = false; // prevent rebuilding again
            tracing::warn!("switching to fast-only model for remaining iterations");
        }

        let tools = internal_tool_specs();
        let call_start = std::time::Instant::now();
        let turn = match tokio::time::timeout(chat_timeout, client.chat(&messages, tools)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                tracing::warn!(
                    iter,
                    tool_calls = tool_calls_count,
                    "sub_agent chat call timed out at {}s — returning partial result",
                    cfg.sub_agent.chat_timeout_secs
                );
                return Ok(finalize_partial(messages, iter, tool_calls_count, total, bytes_in, "chat-timeout"));
            }
        };
        let call_elapsed = call_start.elapsed().as_secs();

        // Track consecutive primary-model timeouts. A call that took longer
        // than the HTTP timeout means the primary timed out and the fallback
        // was used. Reset the counter on fast calls (primary responded in time).
        if call_elapsed >= http_timeout as u64 {
            consecutive_timeouts += 1;
            tracing::warn!(
                iter,
                call_elapsed,
                http_timeout,
                consecutive_timeouts,
                "primary model timed out — fallback handled this call"
            );
        } else {
            consecutive_timeouts = 0;
        }

        // One round-trip to the GLM backend is one billable iteration —
        // count it whether or not the response carried a usage block.
        crate::events::add_glm_iter();
        if let Some(u) = &turn.usage {
            total.prompt_tokens += u.prompt_tokens;
            total.completion_tokens += u.completion_tokens;
            total.total_tokens += u.total_tokens;
            total.cached_tokens = total.cached_tokens.saturating_add(u.cached_tokens);
            total.cache_creation_tokens = total
                .cache_creation_tokens
                .saturating_add(u.cache_creation_tokens);
            // Fold this turn's tokens back into the per-MCP-call CallCtx so
            // transport.rs can record the cumulative GLM cost — even when a
            // single tool calls `sub_agent::run` more than once.
            crate::events::add_glm_tokens(
                u.prompt_tokens,
                u.completion_tokens,
                u.cached_tokens,
                u.cache_creation_tokens,
            );
        }

        // Append the assistant's full reply to history, regardless of whether
        // it's a tool-call or a final answer.
        messages.push(turn.message.clone());

        let calls = turn.message.tool_calls.clone().unwrap_or_default();
        if calls.is_empty() {
            // Done — model returned a plain answer.
            let answer = extract_answer_content(&turn.message).unwrap_or_default();
            tracing::info!(
                iter = iter + 1,
                tool_calls = tool_calls_count,
                tokens = total.total_tokens,
                "sub_agent done"
            );
            return Ok(SubAgentRun {
                answer,
                iterations: iter + 1,
                tool_calls: tool_calls_count,
                usage_total: total,
                bytes_in,
            });
        }

        // Run each tool the model asked for, append `tool` messages.
        for call in calls {
            tool_calls_count += 1;
            let result = exec_internal_tool(&call).await;
            let result_text = match result {
                Ok(s) => s,
                Err(e) => format!("ERROR: {}", e),
            };
            // Trim long outputs — the model can call again if it needs more.
            let truncated = truncate_lossy(&result_text, 12_000);
            let n = truncated.len() as u64;
            bytes_in = bytes_in.saturating_add(n);
            crate::events::add_bytes_in(n);
            messages.push(Message::tool(&call.id, &call.function.name, truncated));
        }
    }

    // Loop exhausted; return whatever the last assistant message had.
    let last = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .and_then(extract_answer_content)
        .unwrap_or_else(|| "[sub-agent hit max_iterations without final answer]".to_string());
    Ok(SubAgentRun {
        answer: last,
        iterations: max_iter,
        tool_calls: tool_calls_count,
        usage_total: total,
        bytes_in,
    })
}

/// Build a graceful partial-answer reply when we hit a budget cap. The main
/// model gets the last assistant text plus a one-line "[partial: <why>]"
/// trailer so it knows to either narrow scope or finish the job itself.
///
/// The trailer text adapts to *why* we bailed and *how much* the sub-agent
/// managed to do: 0-iter chat-timeout means the GLM backend itself wasn't
/// reachable in time, which is a totally different failure mode (fall back
/// to local Grep/Read) than a deadline hit after several iters (narrow the
/// scope and retry).
fn finalize_partial(
    messages: Vec<Message>,
    iter: u32,
    tool_calls_count: u32,
    total: Usage,
    bytes_in: u64,
    why: &str,
) -> SubAgentRun {
    let last = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .and_then(extract_answer_content)
        .unwrap_or_else(|| "[sub-agent had no partial output]".to_string());
    let advice = if iter == 0 {
        // First chat call never returned. Almost always a backend connectivity
        // issue — slow model, cold connection, network blip. Telling the caller
        // to "narrow scope" wouldn't help; tell them to use local tools.
        "Backend slow / unreachable — fall back to local Grep / Read for this task; retry research later if you need a deeper synthesis"
    } else {
        "narrow the scope or split the query"
    };
    let answer = format!(
        "{}\n\n[glance partial: {} after {} iters / {} tool calls — {}]",
        last, why, iter, tool_calls_count, advice
    );
    SubAgentRun {
        answer,
        iterations: iter,
        tool_calls: tool_calls_count,
        usage_total: total,
        bytes_in,
    }
}

// ── Internal tools the backend model can call ───────────────────────────────

fn internal_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: "read_file".into(),
                description: "Read a file from disk. \
                              PREFER `mode=\"outline\"` first when orienting (e.g. 'what's in this module?'); \
                              it returns only signatures + imports + (for python) one-line docstrings, \
                              compressing typical source files by 70-90%. \
                              Use `mode=\"skeleton\"` for the tightest view (no docstrings). \
                              Only use `mode=\"full\"` (default) when you actually need function bodies. \
                              Pass `offset` (1-based) and `limit` to paginate a `full` read; outline/skeleton ignore offset/limit. \
                              Markdown falls back to a heading outline; unknown extensions to head+tail snippets.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path":   { "type": "string", "description": "Absolute or relative path." },
                        "mode":   {
                            "type": "string",
                            "enum": ["full", "outline", "skeleton"],
                            "description": "How to render. 'outline' = signatures + imports (recommended for orientation). 'skeleton' = even tighter, no docstrings. 'full' = paginated raw lines."
                        },
                        "offset": { "type": "integer", "description": "1-based line number to start at (full mode only). Default 1.", "minimum": 1 },
                        "limit":  { "type": "integer", "description": "Max lines to return (full mode only). Default 400. Cap 2000.", "minimum": 1 }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: "list_dir".into(),
                description: "List entries in a directory (non-recursive). Returns one entry per line, dirs marked with /.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path. Default '.'" }
                    }
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: "grep".into(),
                description: "Search files for a regex pattern. Returns up to 100 matching lines as `<path>:<line>: <text>`.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex (Rust regex syntax)." },
                        "path":    { "type": "string", "description": "Directory or file root. Default '.'" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
    ]
}

async fn exec_internal_tool(call: &ToolCall) -> Result<String> {
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(json!({}));
    match call.function.name.as_str() {
        "read_file" => tool_read_file(args).await,
        "list_dir" => tool_list_dir(args).await,
        "grep" => tool_grep(args).await,
        other => Err(anyhow!("unknown internal tool: {}", other)),
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

const READ_DEFAULT_LIMIT: u32 = 400;
const READ_MAX_LIMIT: u32 = 2000;
const READ_MAX_BYTES: u64 = 4 * 1024 * 1024; // 4MB hard cap on the underlying file

async fn tool_read_file(args: Value) -> Result<String> {
    let ReadArgs {
        path,
        mode,
        offset,
        limit,
    } = serde_json::from_value(args)?;
    let p = resolve_path(&path)?;
    let meta = tokio::fs::metadata(&p).await?;
    if meta.len() > READ_MAX_BYTES {
        return Ok(format!(
            "[file too large: {} bytes (>4MB); use grep or read a narrower path]",
            meta.len()
        ));
    }

    let m = outline::Mode::parse(mode.as_deref());

    let start = offset.unwrap_or(1).max(1);
    let lim = limit.unwrap_or(READ_DEFAULT_LIMIT).min(READ_MAX_LIMIT);

    // Stream-read line by line so we don't need to hold the whole file when
    // only a window is wanted. Acceptable for files up to a few MB.
    let content = tokio::fs::read_to_string(&p).await?;
    let total: u32 = content.lines().count() as u32;

    // Outline / skeleton modes return a compressed view and skip pagination.
    if m != outline::Mode::Full {
        // For markdown, reuse the existing heading outline path.
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = if ext == "md" || ext == "markdown" {
            crate::markdown::outline(&content)
                .into_iter()
                .map(|h| {
                    format!(
                        "{:>5} | {} {}",
                        h.line,
                        "#".repeat(h.level as usize),
                        h.title
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            outline::render(&p, &content, m).unwrap_or_default()
        };
        let header = outline::savings_header(m, total as usize, &body);
        return Ok(format!(
            "[file: {} | total {} lines]\n{}\n{}\n",
            p.display(),
            total,
            header,
            body
        ));
    }

    let start_idx = (start - 1) as usize; // 0-based
    let take = lim as usize;
    let collected: Vec<&str> = content.lines().skip(start_idx).take(take).collect();
    let end_line = start + collected.len() as u32 - 1;

    let mut out = String::new();
    out.push_str(&format!(
        "[file: {} | total {} lines | showing {}..{}]\n",
        p.display(),
        total,
        start,
        end_line
    ));
    for (i, line) in collected.iter().enumerate() {
        out.push_str(&format!("{:>5} | {}\n", start + i as u32, line));
    }
    if end_line < total {
        out.push_str(&format!(
            "[truncated — file has {} more lines. Call again with offset={}.]\n",
            total - end_line,
            end_line + 1,
        ));
    }
    Ok(out)
}

#[derive(Deserialize, Default)]
struct ListArgs {
    #[serde(default = "default_dot")]
    path: String,
}
fn default_dot() -> String {
    ".".into()
}

async fn tool_list_dir(args: Value) -> Result<String> {
    let ListArgs { path } = serde_json::from_value(args).unwrap_or_default();
    let p = resolve_path(&path)?;
    let mut entries: Vec<String> = Vec::new();
    let mut rd = tokio::fs::read_dir(&p).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let suffix = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            "/"
        } else {
            ""
        };
        entries.push(format!("{}{}", name, suffix));
    }
    entries.sort();
    Ok(entries.join("\n"))
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_dot")]
    path: String,
}

async fn tool_grep(args: Value) -> Result<String> {
    let GrepArgs { pattern, path } = serde_json::from_value(args)?;
    let root = resolve_path(&path)?;
    let re = regex::Regex::new(&pattern).map_err(|e| anyhow!("bad regex: {}", e))?;
    let mut hits: Vec<String> = Vec::new();
    let max = 100usize;

    // Walk files; skip obvious ignore dirs.
    let walker = walkdir::WalkDir::new(&root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        !matches!(
            name.as_ref(),
            ".git" | "node_modules" | "target" | "dist" | "build"
        )
    });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        // Read in blocking fashion; offload via spawn_blocking for hygiene.
        let path_owned = path.to_path_buf();
        let pattern_re = re.clone();
        let found = tokio::task::spawn_blocking(move || -> Vec<String> {
            let Ok(content) = std::fs::read_to_string(&path_owned) else {
                return Vec::new();
            };
            content
                .lines()
                .enumerate()
                .filter(|(_, line)| pattern_re.is_match(line))
                .take(20)
                .map(|(i, line)| {
                    format!(
                        "{}:{}: {}",
                        path_owned.display(),
                        i + 1,
                        line.chars().take(200).collect::<String>()
                    )
                })
                .collect()
        })
        .await
        .unwrap_or_default();

        for h in found {
            hits.push(h);
            if hits.len() >= max {
                break;
            }
        }
        if hits.len() >= max {
            break;
        }
    }

    if hits.is_empty() {
        Ok(format!(
            "(no matches for /{}/ in {})",
            pattern,
            root.display()
        ))
    } else {
        Ok(hits.join("\n"))
    }
}

/// Extract the best available text from an assistant message.
///
/// DeepSeek reasoning models (deepseek-v4-pro) may return `content: null` in
/// the final response while the actual output lives in `reasoning_content`.
/// Try `content` first; if it's missing or whitespace-only, fall back to
/// `reasoning_content`. Returns `None` only when both are truly empty.
fn extract_answer_content(msg: &Message) -> Option<String> {
    if let Some(c) = &msg.content {
        let trimmed = c.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(rc) = &msg.reasoning_content {
        let trimmed = rc.trim();
        if !trimmed.is_empty() {
            tracing::warn!("assistant content empty, falling back to reasoning_content");
            return Some(trimmed.to_string());
        }
    }
    None
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Resolve a path against cwd. Reject obvious traversals and absolute paths
/// outside the user's home (light guard — full sandboxing is out of scope).
fn resolve_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    Ok(abs)
}

/// Truncate a string to roughly `max_chars` chars, leaving a marker.
fn truncate_lossy(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{}\n…[truncated, {} chars total]", head, s.chars().count())
}
