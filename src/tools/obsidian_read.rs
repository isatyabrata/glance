//! `obsidian_read` — read one note from the configured vault and return its
//! frontmatter, body, wikilinks, and tags. No side effects, no sub-agent.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};
use crate::obsidian;

#[derive(Debug, Deserialize)]
struct Args {
    /// Note name (without `.md`) or relative path within the vault.
    note: String,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "obsidian_read".into(),
        description:
            "Read one note from the configured Obsidian vault. Returns frontmatter, body, \
             wikilinks, and tags as a JSON-encoded text block. Read-only."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Note name (without .md) or relative path within the vault. Nested paths like 'Folder/Note' work.",
                }
            },
            "required": ["note"],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { note } = serde_json::from_value(args)?;
    let cfg = config::load_or_default()?;
    let vault = obsidian::resolve_vault(&cfg)?;
    let path = obsidian::note_path_for_name(&vault, &note);

    if !path.exists() {
        return Ok(CallToolResult::error(format!(
            "note not found: {}",
            path.display()
        )));
    }

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;

    let (frontmatter, body) = obsidian::parse_frontmatter(&raw);
    let wikilinks: Vec<Value> = obsidian::extract_wikilinks(&body)
        .into_iter()
        .map(|w| {
            json!({
                "target": w.target,
                "alias": w.alias,
            })
        })
        .collect();
    let tags = obsidian::extract_tags(&body);
    let rel = path
        .strip_prefix(&vault)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();

    let payload = json!({
        "path": rel,
        "frontmatter": frontmatter,
        "body": body,
        "wikilinks": wikilinks,
        "tags": tags,
    });
    Ok(CallToolResult::text(serde_json::to_string_pretty(
        &payload,
    )?))
}
