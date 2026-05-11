//! `obsidian_backlinks` — find notes that wikilink-reference a target note.
//! Walks the vault for `*.md` files and matches `[[<note>]]` (and aliased
//! variants `[[<note>|...]]`) case-insensitively on the bare name.

use anyhow::Result;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::config;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};
use crate::obsidian;

const MAX_HITS: usize = 200;
const CONTEXT_MAX: usize = 200;

#[derive(Debug, Deserialize)]
struct Args {
    /// Target note name (without `.md`) or relative path within the vault.
    note: String,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "obsidian_backlinks".into(),
        description:
            "Find notes that link to a given note via [[wikilink]]. Returns 'relative_path:line — context' lines. \
             Match is case-insensitive on the bare note name (without .md)."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Target note name (without .md). Folder paths are reduced to the file basename for matching.",
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

    // Reduce to the bare basename (no extension, no folder prefix).
    let bare = std::path::Path::new(&note)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| note.clone());

    // Match `[[bare]]` or `[[bare|...]]` or `[[bare#...]]` — case-insensitive,
    // tolerant of surrounding whitespace.
    let pattern = format!(r"\[\[\s*{}\s*(?:[\|#].*?)?\s*\]\]", regex::escape(&bare));
    let regex = RegexBuilder::new(&pattern).case_insensitive(true).build()?;

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

        // Don't list the note itself as its own backlink.
        if rel.trim_end_matches(".md").eq_ignore_ascii_case(&bare)
            || rel.eq_ignore_ascii_case(&format!("{}.md", bare))
        {
            continue;
        }

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
            "no backlinks to [[{}]]",
            bare
        )));
    }
    Ok(CallToolResult::text(hits.join("\n")))
}
