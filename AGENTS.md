# Project agents — glance

This is the glance MCP server itself. Routing rules from
`~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` apply here too.

## Local conventions

- Rust workspace: top-level crate (`glance`) + `glance-app` Tauri sub-crate.
  `cargo install --path . --bin glance-mcp` after MCP server changes;
  `cd glance-app && npx tauri build --bundles app` after GUI changes.
- Format: `cargo fmt --all` before commits. CI fails on `clippy --workspace
  -- -D warnings` so run that locally.
- Tests live in `#[cfg(test)] mod tests` blocks per file. `cargo test --lib
  -p glance` for the MCP core; the `glance-app` sub-crate has none yet.

## Wire protocol

- MCP stdio. Each tool implementation lives in `src/tools/<name>.rs` and
  registers in `src/tools/mod.rs` (both `list_enabled` and `dispatch`).
- Sub-agent loop is in `src/mcp/sub_agent.rs`. Every tool that drives an
  LLM goes through it, so changes there hit ALL text tools at once.
- Backend HTTP retry + fallback_models live in
  `src/backend/openai_compat.rs`. Vision is a sibling
  `src/backend/vision.rs`.

## When editing this project

Don't dogfood glance to read this codebase — you're already running INSIDE
the test target. `Read` / `Grep` / `Edit` directly. Only use glance for
external context (other repos, web pages).
