//! `md_read` — read a markdown file, split frontmatter from body.
//!
//! Direct file IO, no sub-agent. The MCP caller gets back a single text block
//! containing pretty-printed JSON with two fields:
//! - `frontmatter`: the parsed YAML as JSON (or `null` if absent)
//! - `body`: the markdown body, optionally windowed by `offset`/`limit`.
//!
//! Mirrors the windowing behaviour of `sub_agent`'s `read_file` (default 400
//! lines, max 2000) so large notes don't blow up the response.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const READ_DEFAULT_LIMIT: u32 = 400;
const READ_MAX_LIMIT: u32 = 2000;
const READ_MAX_BYTES: u64 = 4 * 1024 * 1024; // 4MB hard cap.

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "md_read".into(),
        description:
            "Read a Markdown file and split YAML frontmatter from body. Returns JSON with \
             `frontmatter` (object|null) and `body` (string). Use `offset`/`limit` to window \
             long notes (default 400 lines, max 2000). Direct file IO — no sub-agent."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string", "description": "Absolute or cwd-relative path to a .md file." },
                "offset": { "type": "integer", "description": "1-based line offset into the body. Default 1.", "minimum": 1 },
                "limit":  { "type": "integer", "description": "Max body lines to return. Default 400. Cap 2000.", "minimum": 1 }
            },
            "required": ["path"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        path,
        offset,
        limit,
    } = serde_json::from_value(args)?;
    let p = resolve_path(&path)?;

    let meta = tokio::fs::metadata(&p)
        .await
        .with_context(|| format!("stat {}", p.display()))?;
    if meta.len() > READ_MAX_BYTES {
        return Ok(CallToolResult::error(format!(
            "[file too large: {} bytes (>4MB); read a narrower path]",
            meta.len()
        )));
    }

    let raw = tokio::fs::read_to_string(&p)
        .await
        .with_context(|| format!("read {}", p.display()))?;

    let (fm_yaml, body_full) = crate::markdown::parse_frontmatter(&raw);

    // Window the body. Line numbers are relative to the body, not the file.
    let total_body_lines: u32 = body_full.lines().count() as u32;
    let start = offset.unwrap_or(1).max(1);
    let lim = limit.unwrap_or(READ_DEFAULT_LIMIT).min(READ_MAX_LIMIT);

    let start_idx = (start - 1) as usize;
    let take = lim as usize;
    let collected: Vec<&str> = body_full.lines().skip(start_idx).take(take).collect();
    let body_window = collected.join("\n");
    let end_line = if collected.is_empty() {
        start.saturating_sub(1)
    } else {
        start + collected.len() as u32 - 1
    };
    let truncated = end_line < total_body_lines;

    // Convert YAML → JSON for the response. Failure (e.g. value not
    // expressible in JSON) degrades to `null` rather than erroring out.
    let fm_json: Value = match fm_yaml {
        Some(v) => serde_json::to_value(&v).unwrap_or(Value::Null),
        None => Value::Null,
    };

    let out = json!({
        "path": p.display().to_string(),
        "frontmatter": fm_json,
        "body": body_window,
        "body_lines_total": total_body_lines,
        "body_lines_shown": format!("{}..{}", start, end_line),
        "truncated": truncated,
    });

    Ok(CallToolResult::text(serde_json::to_string_pretty(&out)?))
}

fn resolve_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}
