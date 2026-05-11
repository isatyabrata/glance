//! `research` — multi-file research + summary, no side effects.
//!
//! The MCP client (codex/claude) gives us a query and a scope (file paths or
//! globs). We hand it to the sub-agent, which is allowed to read/grep/list
//! freely. The sub-agent returns a compressed summary that the MCP client can
//! reason about without burning its own context window on the original files.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::{
    protocol::{CallToolResult, ToolDefinition},
    sub_agent,
};

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    /// File paths or globs the sub-agent should focus on. Empty = whole cwd.
    #[serde(default)]
    scope: Vec<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "research".into(),
        description:
            "Read multiple files / grep / list directories and return a compressed summary. \
             Use this BEFORE editing code to orient — saves the calling model from burning \
             its own context on file contents. Returns a few paragraphs of text, not raw \
             file content. No side effects."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What you want to find out. E.g. 'where is login validation?', 'what does this module export?'",
                },
                "scope": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "File paths or globs to focus on. Empty = whole working directory.",
                },
            },
            "required": ["query"],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { query, scope } = serde_json::from_value(args)?;

    let scope_str = if scope.is_empty() {
        "(whole working directory)".to_string()
    } else {
        scope.join(", ")
    };

    let system = format!(
        "You are glance's research sub-agent. The user is busy and asked you to look into a \
         codebase so they don't have to. You can call tools to read files / grep / list \
         directories. Once you have enough info, return a SHORT summary — at most 3 paragraphs. \
         Never dump raw file contents back. Cite file paths and line numbers when relevant.\n\n\
         Scope: {}",
        scope_str
    );

    let run = sub_agent::run(&system, &query).await?;
    tracing::info!(
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        "research completed"
    );
    Ok(CallToolResult::text(run.answer))
}
