//! `md_write` — propose a Markdown file change WITHOUT touching disk.
//!
//! glance never writes the user's notes itself. Instead it drops a patch file
//! under `~/.glance/patches/<ts>-md_write-<basename>.patch` and hands the path
//! back to the MCP caller (codex/claude). The caller decides whether to apply.
//!
//! Patch shape:
//!
//! ```text
//! {"target":"/abs/path","mode":"overwrite","reason":"..."}
//! ---
//! <full file content the sub-agent wants to write>
//! ```

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    content: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    reason: Option<String>,
}

fn default_mode() -> String {
    "create".into()
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "md_write".into(),
        description:
            "Propose a Markdown write WITHOUT touching disk. Writes the proposed change to \
             `~/.glance/patches/<ts>-md_write-<basename>.patch` and returns the patch path. \
             The MCP caller decides whether to apply it. Modes: `create` (fail if exists), \
             `overwrite`, `append`. Default `create`."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string",  "description": "Absolute or cwd-relative target file path." },
                "content": { "type": "string",  "description": "Full file content (for create/overwrite) or text to append." },
                "mode":    { "type": "string",  "enum": ["create", "overwrite", "append"], "description": "Default `create`." },
                "reason":  { "type": "string",  "description": "Short justification for the change. Surfaced in the patch header." }
            },
            "required": ["path", "content"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        path,
        content,
        mode,
        reason,
    } = serde_json::from_value(args)?;
    let target = resolve_path(&path)?;

    if !matches!(mode.as_str(), "create" | "overwrite" | "append") {
        return Ok(CallToolResult::error(format!(
            "invalid mode `{}`; expected create|overwrite|append",
            mode
        )));
    }

    // Light pre-flight so the caller learns about obvious mistakes immediately,
    // even though we never actually write the file.
    let exists = tokio::fs::metadata(&target).await.is_ok();
    if mode == "create" && exists {
        return Ok(CallToolResult::error(format!(
            "create mode but target already exists: {}",
            target.display()
        )));
    }
    if mode == "append" && !exists {
        return Ok(CallToolResult::error(format!(
            "append mode but target does not exist: {}",
            target.display()
        )));
    }

    let patch_dir = patches_dir().context("locate patches dir")?;
    tokio::fs::create_dir_all(&patch_dir)
        .await
        .with_context(|| format!("mkdir -p {}", patch_dir.display()))?;

    let basename = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let patch_name = format!("{}-md_write-{}.patch", ts, sanitize(&basename));
    let patch_path = patch_dir.join(&patch_name);

    let header = json!({
        "target": target.display().to_string(),
        "mode": mode,
        "reason": reason.unwrap_or_default(),
    });

    let mut body = String::new();
    body.push_str(&serde_json::to_string(&header)?);
    body.push('\n');
    body.push_str("---\n");
    body.push_str(&content);
    if !content.ends_with('\n') {
        body.push('\n');
    }

    tokio::fs::write(&patch_path, body.as_bytes())
        .await
        .with_context(|| format!("write {}", patch_path.display()))?;

    let summary = json!({
        "patch": patch_path.display().to_string(),
        "target": target.display().to_string(),
        "mode": mode,
        "applied": false,
        "note": "glance does not apply the patch; the MCP caller decides.",
    });
    Ok(CallToolResult::text(serde_json::to_string_pretty(
        &summary,
    )?))
}

fn resolve_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn patches_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("patches"))
}

/// Strip path separators and other awkward characters from a basename so the
/// patch filename is always a safe single segment.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}
