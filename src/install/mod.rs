//! Install / remove glance MCP server entries in client config files.
//!
//! Supported clients:
//! - codex CLI    → `~/.codex/config.toml`
//! - Claude Code  → `~/.claude.json`
//! - Cursor       → `~/.cursor/mcp.json`
//!
//! All write paths produce a `<file>.bak.<ts>` backup of the original first.

use anyhow::{anyhow, Result};

pub mod chrome;
pub mod chrome_adapter_import;
pub mod chrome_adapters;
pub mod claude;
pub mod codex;
pub mod cursor;

/// Which clients this install run targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Codex,
    Claude,
    Cursor,
    All,
}

impl Client {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "codex" => Ok(Client::Codex),
            "claude" => Ok(Client::Claude),
            "cursor" => Ok(Client::Cursor),
            "all" => Ok(Client::All),
            other => Err(anyhow!(
                "unknown --client value '{}': expected codex|claude|cursor|all",
                other
            )),
        }
    }
}

/// Top-level orchestrator. `client` is one of `codex` / `claude` / `cursor` /
/// `all`. When `remove` is true, removes the glance entry rather than writing.
pub fn install(client: &str, remove: bool) -> Result<()> {
    install_with_opts(client, remove, true)
}

/// Like [`install`] plus `agents_md_hint`: when true (and we're not removing),
/// also append a one-line glance hint to AGENTS.md (codex) / CLAUDE.md (claude)
/// in the current working directory if those files exist. Idempotent.
pub fn install_with_opts(client: &str, remove: bool, agents_md_hint: bool) -> Result<()> {
    let target = Client::parse(client)?;
    let do_one = |c: Client| -> Result<()> {
        one(c, remove)?;
        if !remove && agents_md_hint {
            // Best-effort: don't fail the install if the hint can't be written.
            if let Err(e) = maybe_append_agents_hint(c) {
                tracing::warn!("agents_md hint: {}", e);
            }
        }
        Ok(())
    };
    match target {
        Client::Codex | Client::Claude | Client::Cursor => do_one(target),
        Client::All => {
            let mut errs: Vec<String> = Vec::new();
            for c in [Client::Codex, Client::Claude, Client::Cursor] {
                if let Err(e) = do_one(c) {
                    errs.push(format!("{:?}: {:#}", c, e));
                }
            }
            if errs.is_empty() {
                Ok(())
            } else {
                Err(anyhow!("some clients failed:\n  {}", errs.join("\n  ")))
            }
        }
    }
}

/// The single-line hint we append to AGENTS.md / CLAUDE.md.
const HINT_LINE: &str = "- For multi-file research, prefer `mcp__glance__research(query=..., scope=...)` over reading files yourself; it returns a summary instead of raw bytes.";

/// Append [`HINT_LINE`] to the appropriate project-level instruction file when
/// it exists. Idempotent — checks for the line first. No-op for cursor.
fn maybe_append_agents_hint(client: Client) -> Result<()> {
    let target_name = match client {
        Client::Codex => "AGENTS.md",
        Client::Claude => "CLAUDE.md",
        _ => return Ok(()),
    };
    let cwd = std::env::current_dir()?;
    let path = cwd.join(target_name);
    if !path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(&path)?;
    if existing.contains(HINT_LINE) {
        return Ok(()); // already present, nothing to do
    }
    let mut buf = existing;
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    // Separate from preceding content with a blank line for readability.
    if !buf.ends_with("\n\n") {
        buf.push('\n');
    }
    buf.push_str(HINT_LINE);
    buf.push('\n');
    std::fs::write(&path, buf)?;
    println!("\u{2713} appended glance hint to {}", path.display());
    Ok(())
}

fn one(client: Client, remove: bool) -> Result<()> {
    match client {
        Client::Codex => {
            if remove {
                codex::remove()
            } else {
                codex::install()
            }
        }
        Client::Claude => {
            if remove {
                claude::remove()
            } else {
                claude::install()
            }
        }
        Client::Cursor => {
            if remove {
                cursor::remove()
            } else {
                cursor::install()
            }
        }
        Client::All => unreachable!(),
    }
}

/// Make a `<path>.bak.<unix-ts>` copy of `path` if it exists. Returns the
/// backup path, or `None` if the original didn't exist.
pub(crate) fn backup_if_exists(path: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".bak.{}", ts));
    let backup = std::path::PathBuf::from(backup);
    std::fs::copy(path, &backup)?;
    Ok(Some(backup))
}

/// Resolve `~/<rest>` to an absolute path. Errors if there's no home dir.
pub(crate) fn home_join(rest: &str) -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home.join(rest))
}
