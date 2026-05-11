//! `obsidian_search` — ripgrep-style scan over `*.md` files in the vault.
//! Returns up to 50 hits as `relative_path:line — context`.

use anyhow::Result;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::config;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};
use crate::obsidian;

const MAX_HITS: usize = 50;
const CONTEXT_MAX: usize = 200;

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    /// `"text"` (default, case-insensitive substring) or `"regex"`.
    #[serde(default)]
    mode: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "obsidian_search".into(),
        description:
            "Search the Obsidian vault for a query across all .md files. Returns up to 50 \
             hits as 'relative_path:line — context'. mode='text' (default, case-insensitive) \
             or mode='regex' for regular expression matching. Read-only."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to search for. Plain text or regex depending on mode.",
                },
                "mode": {
                    "type": "string",
                    "enum": ["text", "regex"],
                    "description": "Match mode. Default 'text' (case-insensitive substring).",
                }
            },
            "required": ["query"],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { query, mode } = serde_json::from_value(args)?;
    let cfg = config::load_or_default()?;
    let vault = obsidian::resolve_vault(&cfg)?;

    let mode = mode.as_deref().unwrap_or("text");
    let regex = match mode {
        "regex" => RegexBuilder::new(&query).build()?,
        _ => RegexBuilder::new(&regex::escape(&query))
            .case_insensitive(true)
            .build()?,
    };

    let mut hits: Vec<String> = Vec::new();
    'outer: for entry in WalkDir::new(&vault)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if !entry
            .path()
            .extension()
            .map(|x| x.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
        {
            continue;
        }
        // Skip hidden dot-dirs (e.g. `.obsidian`, `.trash`).
        if entry
            .path()
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }

        let raw = match std::fs::read_to_string(entry.path()) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = entry
            .path()
            .strip_prefix(&vault)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        for (idx, line) in raw.lines().enumerate() {
            if regex.is_match(line) {
                let context = if line.len() > CONTEXT_MAX {
                    format!("{}…", &line[..CONTEXT_MAX])
                } else {
                    line.to_string()
                };
                hits.push(format!("{}:{} — {}", rel, idx + 1, context.trim()));
                if hits.len() >= MAX_HITS {
                    break 'outer;
                }
            }
        }
    }

    if hits.is_empty() {
        return Ok(CallToolResult::text(format!(
            "no matches for {:?} in {}",
            query,
            vault.display()
        )));
    }
    Ok(CallToolResult::text(hits.join("\n")))
}
