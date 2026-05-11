#![recursion_limit = "512"]
//! `glance` — MCP server that delegates research / markdown / obsidian work to
//! a cheap sub-agent (GLM, DeepSeek, etc) so codex / claude / cursor save tokens
//! on the heavy "read N files and figure out what's there" loop.
//!
//! Architecture:
//!
//! ```text
//! codex CLI / claude / cursor
//!         │ (stdio JSON-RPC, MCP protocol)
//!         ▼
//!   glance-mcp (this binary, --mcp-stdio mode)
//!         │
//!    ┌────┴────┐
//!    │ tools/  │── research, explain, search, md_*, obsidian_*, write_*
//!    └────┬────┘
//!         │
//!         ▼
//!   sub_agent loop (function calling)
//!         │
//!         ▼
//!   OpenAI-compat backend (GLM / DeepSeek / OpenAI / ...)
//! ```

pub mod backend;
pub mod config;
pub mod events;
pub mod install;
pub mod markdown;
pub mod mcp;
pub mod mcp_aggregator;
pub mod obsidian;
pub mod safety;
pub mod tools;
