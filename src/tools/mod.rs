//! Tools exposed to MCP clients (codex / claude / cursor).
//!
//! Each tool has its own module with a `definition()` returning the JSON-Schema
//! and a `call(args)` returning a [`CallToolResult`]. The tool registry below
//! filters by what's enabled in [`crate::config`].

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

pub mod chrome;
pub mod explain;
pub mod fix_lint;
pub mod image_describe;
pub mod md_outline;
pub mod md_read;
pub mod md_write;
pub mod obsidian_backlinks;
pub mod obsidian_read;
pub mod obsidian_search;
pub mod obsidian_write;
pub mod repo_explore;
pub mod research;
pub mod search;
pub mod web_fetch;
pub mod web_search;
pub mod write_docs;
pub mod write_tests;

/// True iff the given builtin tool is exposed to the current MCP client.
/// Mirrors `UpstreamState::allowed_for`: empty / missing list = visible to
/// every client; non-empty = only those clients see it.
fn allowed_for_current_client(cfg: &crate::config::Config, tool: &str) -> bool {
    let allow = match cfg.tools_clients.get(tool) {
        Some(v) if !v.is_empty() => v,
        _ => return true,
    };
    let client = crate::mcp::current_client();
    allow.iter().any(|c| c.eq_ignore_ascii_case(client))
}

/// Return the tools that are enabled in the current config.
pub async fn list_enabled() -> Result<Vec<ToolDefinition>> {
    let cfg = crate::config::load_or_default()?;
    let mut out = Vec::new();
    let push = |out: &mut Vec<ToolDefinition>, def: ToolDefinition| {
        if allowed_for_current_client(&cfg, &def.name) {
            out.push(def);
        }
    };
    if cfg.tools.research {
        push(&mut out, research::definition());
    }
    if cfg.tools.explain {
        push(&mut out,explain::definition());
    }
    if cfg.tools.search {
        push(&mut out,search::definition());
    }
    if cfg.tools.md_read {
        push(&mut out,md_read::definition());
    }
    if cfg.tools.md_outline {
        push(&mut out,md_outline::definition());
    }
    if cfg.tools.md_write {
        push(&mut out,md_write::definition());
    }
    if cfg.tools.obsidian_read {
        push(&mut out,obsidian_read::definition());
    }
    if cfg.tools.obsidian_search {
        push(&mut out,obsidian_search::definition());
    }
    if cfg.tools.obsidian_backlinks {
        push(&mut out,obsidian_backlinks::definition());
    }
    if cfg.tools.obsidian_write {
        push(&mut out,obsidian_write::definition());
    }
    if cfg.tools.write_tests {
        push(&mut out,write_tests::definition());
    }
    if cfg.tools.write_docs {
        push(&mut out,write_docs::definition());
    }
    if cfg.tools.fix_lint {
        push(&mut out,fix_lint::definition());
    }
    if cfg.tools.web_fetch {
        push(&mut out,web_fetch::definition());
    }
    if cfg.tools.repo_explore {
        push(&mut out,repo_explore::definition());
    }
    if cfg.tools.image_describe {
        push(&mut out,image_describe::definition());
    }
    if cfg.tools.web_search {
        push(&mut out,web_search::definition());
    }
    if cfg.tools.chrome {
        push(&mut out,chrome::definition());
    }
    Ok(out)
}

/// Dispatch a `tools/call` to the matching tool implementation.
pub async fn dispatch(name: &str, args: Value) -> Result<CallToolResult> {
    match name {
        "research" => research::call(args).await,
        "explain" => explain::call(args).await,
        "search" => search::call(args).await,
        "md_read" => md_read::call(args).await,
        "md_outline" => md_outline::call(args).await,
        "md_write" => md_write::call(args).await,
        "obsidian_read" => obsidian_read::call(args).await,
        "obsidian_search" => obsidian_search::call(args).await,
        "obsidian_backlinks" => obsidian_backlinks::call(args).await,
        "obsidian_write" => obsidian_write::call(args).await,
        "write_tests" => write_tests::call(args).await,
        "write_docs" => write_docs::call(args).await,
        "fix_lint" => fix_lint::call(args).await,
        "web_fetch" => web_fetch::call(args).await,
        "repo_explore" => repo_explore::call(args).await,
        "image_describe" => image_describe::call(args).await,
        "web_search" => web_search::call(args).await,
        "chrome" => chrome::call(args).await,
        other => Err(anyhow!("unknown tool: {}", other)),
    }
}
