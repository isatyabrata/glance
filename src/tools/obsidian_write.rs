//! `obsidian_write` — patch-mode writer. Never touches the vault directly;
//! emits a patch file under `~/.glance/patches/` for the user to review.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};
use crate::obsidian;

#[derive(Debug, Deserialize)]
struct Args {
    /// Note name (without `.md`) or relative path within the vault.
    note: String,
    /// Body content to write (without frontmatter — pass `frontmatter` separately).
    content: String,
    /// "create" | "overwrite" | "append" (default "create").
    #[serde(default)]
    mode: Option<String>,
    /// Optional YAML frontmatter as a JSON object. Will be serialized with
    /// `serde_yaml`.
    #[serde(default)]
    frontmatter: Option<Value>,
    /// Optional list of tags. Merged into frontmatter under the `tags:` key.
    #[serde(default)]
    tags: Option<Vec<String>>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "obsidian_write".into(),
        description:
            "PATCH-MODE writer for the Obsidian vault. Does NOT modify the vault. Writes a \
             patch file under ~/.glance/patches/<timestamp>-obsidian_write-<basename>.patch \
             with a JSON metadata header and the rendered note body. Use mode=create / \
             overwrite / append to express intent. Optional `frontmatter` (object) and `tags` \
             (string[]) get prepended as a YAML --- block."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Note name or relative path within the vault.",
                },
                "content": {
                    "type": "string",
                    "description": "Body markdown (without frontmatter).",
                },
                "mode": {
                    "type": "string",
                    "enum": ["create", "overwrite", "append"],
                    "description": "Intended write mode. Default 'create'.",
                },
                "frontmatter": {
                    "type": "object",
                    "description": "Optional YAML frontmatter as a JSON object.",
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tag list. Merged into frontmatter under 'tags'.",
                }
            },
            "required": ["note", "content"],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        note,
        content,
        mode,
        frontmatter,
        tags,
    } = serde_json::from_value(args)?;
    let mode = mode.as_deref().unwrap_or("create").to_string();
    if !matches!(mode.as_str(), "create" | "overwrite" | "append") {
        return Ok(CallToolResult::error(format!(
            "invalid mode {:?} — must be create | overwrite | append",
            mode
        )));
    }

    let cfg = config::load_or_default()?;
    let vault = obsidian::resolve_vault(&cfg)?;
    let target = obsidian::note_path_for_name(&vault, &note);
    let rel_target = target
        .strip_prefix(&vault)
        .unwrap_or(&target)
        .to_string_lossy()
        .into_owned();

    // Build the rendered note: optional frontmatter + content.
    let rendered = render_note(&content, frontmatter.as_ref(), tags.as_deref())?;

    // Compose patch metadata.
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let basename = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "note".to_string())
        .replace(['/', '\\', ' '], "_");

    let patches_dir = patches_dir()?;
    std::fs::create_dir_all(&patches_dir)
        .with_context(|| format!("create patches dir {}", patches_dir.display()))?;

    let patch_name = format!("{}-obsidian_write-{}.patch", timestamp, basename);
    let patch_path = patches_dir.join(&patch_name);

    let header = json!({
        "tool": "obsidian_write",
        "timestamp": timestamp,
        "vault": vault.display().to_string(),
        "target": rel_target,
        "absolute_target": target.display().to_string(),
        "mode": mode,
        "has_frontmatter": frontmatter.is_some() || tags.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
        "byte_len": rendered.len(),
    });
    let header_str = serde_json::to_string_pretty(&header)?;

    let body = format!(
        "# glance patch — obsidian_write\n{}\n--- BEGIN CONTENT ---\n{}\n--- END CONTENT ---\n",
        header_str, rendered
    );

    std::fs::write(&patch_path, body)
        .with_context(|| format!("write patch {}", patch_path.display()))?;

    let summary = json!({
        "patch_path": patch_path.display().to_string(),
        "target": rel_target,
        "mode": mode,
        "bytes": rendered.len(),
        "note": "Patch written. The vault was NOT modified. Apply manually after review.",
    });
    Ok(CallToolResult::text(serde_json::to_string_pretty(
        &summary,
    )?))
}

fn render_note(
    content: &str,
    frontmatter: Option<&Value>,
    tags: Option<&[String]>,
) -> Result<String> {
    let has_fm = frontmatter.is_some() || tags.map(|t| !t.is_empty()).unwrap_or(false);
    if !has_fm {
        return Ok(content.to_string());
    }

    // Build a serde_json::Map representing the frontmatter, then serialize via
    // serde_yaml.
    let mut map = match frontmatter {
        Some(Value::Object(m)) => m.clone(),
        Some(other) => {
            // Caller passed a non-object — wrap it under "data".
            let mut m = serde_json::Map::new();
            m.insert("data".to_string(), other.clone());
            m
        }
        None => serde_json::Map::new(),
    };

    if let Some(t) = tags {
        if !t.is_empty() {
            // Merge with any existing tags entry.
            let merged: Vec<Value> = match map.remove("tags") {
                Some(Value::Array(arr)) => arr
                    .into_iter()
                    .chain(t.iter().map(|s| Value::String(s.clone())))
                    .collect(),
                Some(Value::String(s)) => std::iter::once(Value::String(s))
                    .chain(t.iter().map(|s| Value::String(s.clone())))
                    .collect(),
                _ => t.iter().map(|s| Value::String(s.clone())).collect(),
            };
            // De-dup while preserving order.
            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<Value> = merged
                .into_iter()
                .filter(|v| match v.as_str() {
                    Some(s) => seen.insert(s.to_string()),
                    None => true,
                })
                .collect();
            map.insert("tags".to_string(), Value::Array(deduped));
        }
    }

    let yaml =
        serde_yaml::to_string(&Value::Object(map)).context("serialize frontmatter to YAML")?;
    Ok(format!("---\n{}---\n{}", yaml, content))
}

fn patches_dir() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("patches"))
}
