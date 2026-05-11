//! `md_outline` — render the heading tree of a markdown file as indented text.
//!
//! Direct file IO. Output looks like:
//!
//! ```text
//! # Title (line 1)
//!   ## Section A (line 12)
//!     ### Subsection (line 24)
//!   ## Section B (line 50)
//! ```
//!
//! Indentation = `(level - 1) * 2` spaces. Headings inside fenced code blocks
//! are skipped (handled by [`crate::markdown::outline`]).

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const READ_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "md_outline".into(),
        description:
            "Return the ATX heading tree of a Markdown file as indented text. Each line is \
             `<indent># Title (line N)`. Headings inside fenced code blocks are ignored. \
             Direct file IO — no sub-agent."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative path to a .md file." }
            },
            "required": ["path"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { path } = serde_json::from_value(args)?;
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

    // Outline operates on the body so frontmatter line counts don't shift it.
    // Compute frontmatter-line offset so the reported line numbers still
    // refer to absolute positions in the original file.
    let (_fm, body) = crate::markdown::parse_frontmatter(&raw);
    let body_offset: u32 = if body.as_ptr() == raw.as_ptr() {
        0
    } else {
        // bytes consumed by frontmatter = raw.len() - body.len()
        let consumed = raw.len() - body.len();
        raw[..consumed].lines().count() as u32
    };

    let headings = crate::markdown::outline(body);
    if headings.is_empty() {
        return Ok(CallToolResult::text(format!(
            "(no headings in {})",
            p.display()
        )));
    }

    let mut out = String::new();
    for h in &headings {
        let indent = "  ".repeat(h.level.saturating_sub(1) as usize);
        let hashes = "#".repeat(h.level as usize);
        let abs_line = h.line + body_offset;
        out.push_str(&format!(
            "{}{} {} (line {})\n",
            indent, hashes, h.title, abs_line
        ));
    }

    Ok(CallToolResult::text(out))
}

fn resolve_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}
