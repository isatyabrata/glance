//! `search` — find references in a codebase, regex or semantic.
//!
//! In `regex` mode the sub-agent leans on grep and returns a tight hit list.
//! In `semantic` mode it grep-bootstraps with likely keywords, then reads
//! neighborhoods of hits and filters to the ones that actually match the
//! concept. Either way the caller gets `file:line — context/rationale`, never
//! raw file dumps.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::{
    protocol::{CallToolResult, ToolDefinition},
    sub_agent,
};

#[derive(Debug, Deserialize)]
struct Args {
    /// What to look for — literal regex or a conceptual phrase.
    pattern: String,
    /// Paths or globs to search in. Empty → defaults to ["."].
    #[serde(default)]
    scope: Vec<String>,
    /// "regex" (default) or "semantic".
    #[serde(default)]
    mode: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "search".into(),
        description:
            "Search a codebase for matches and return a tight `file:line — context` list. \
             `mode=regex` (default) does a literal grep. `mode=semantic` looks for the \
             concept — sub-agent grep-bootstraps then filters by reading neighborhoods. \
             Returns at most 30 hits (regex) / 15 hits (semantic). Never dumps raw files."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "What to look for. Regex in regex mode, conceptual phrase in semantic mode.",
                },
                "scope": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Paths or globs to search in. Default ['.']",
                },
                "mode": {
                    "type": "string",
                    "enum": ["regex", "semantic"],
                    "description": "regex (default) or semantic.",
                },
            },
            "required": ["pattern"],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        pattern,
        scope,
        mode,
    } = serde_json::from_value(args)?;

    let scope = if scope.is_empty() {
        vec![".".to_string()]
    } else {
        scope
    };
    let scope_str = scope.join(", ");
    let mode = mode.unwrap_or_else(|| "regex".to_string());

    let system = match mode.as_str() {
        "semantic" => {
            "You are glance's `search` sub-agent in semantic mode. The pattern is conceptual, \
             not literal. Grep for likely keywords, then read neighborhoods of hits and filter \
             to ones actually matching the concept. Return `file:line — short rationale`, max \
             15 hits. Every cited path MUST come from a grep / list_dir / read_file tool \
             result you saw — never invent file paths from prior knowledge of similar \
             projects. If grep returns 0 hits, the answer is `no matches found for <pattern> \
             in <scope>` (verbatim)."
        }
        _ => {
            "You are glance's `search` sub-agent in regex mode. Use the grep tool to find \
             matches in scope. Return a TIGHT list: `file:line — short context`, max 30 hits. \
             Every cited path MUST come from a grep tool result — never invent file paths. \
             If grep returns 0 hits, the answer is `no matches found for <pattern> in \
             <scope>` (verbatim). No raw file dumps."
        }
    };

    let user = format!("Pattern: {}\nScope: {}\nMode: {}", pattern, scope_str, mode);

    let run = sub_agent::run(system, &user).await?;
    tracing::info!(
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        mode = %mode,
        "search completed"
    );
    Ok(CallToolResult::text(run.answer))
}
