//! codex CLI registration: `~/.codex/config.toml`, `[mcp_servers.glance]` section.

use anyhow::{Context, Result};
use std::path::PathBuf;

use super::{backup_if_exists, home_join};

pub fn config_path() -> Result<PathBuf> {
    home_join(".codex/config.toml")
}

/// Add `[mcp_servers.glance]` section pointing at `glance-mcp`.
pub fn install() -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    backup_if_exists(&path)?;

    let mut doc: toml::Value = if path.exists() {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
    } else {
        toml::Value::Table(toml::value::Table::new())
    };

    let root = doc
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("codex config is not a TOML table"))?;

    let servers_entry = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));

    let servers_tbl = servers_entry
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[mcp_servers] is not a table"))?;

    let mut glance = toml::value::Table::new();
    glance.insert("command".into(), toml::Value::String("glance-mcp".into()));
    glance.insert("startup_timeout_sec".into(), toml::Value::Integer(10));
    glance.insert("tool_timeout_sec".into(), toml::Value::Integer(180));
    servers_tbl.insert("glance".into(), toml::Value::Table(glance));

    let serialized = toml::to_string_pretty(&doc).context("serialize codex config")?;
    std::fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;

    println!("\u{2713} registered glance to codex ({})", path.display());
    Ok(())
}

/// Remove `[mcp_servers.glance]` if present.
pub fn remove() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!(
            "— codex config not found ({}), nothing to remove",
            path.display()
        );
        return Ok(());
    }
    backup_if_exists(&path)?;

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut doc: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let mut changed = false;
    if let Some(root) = doc.as_table_mut() {
        if let Some(servers) = root.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
            if servers.remove("glance").is_some() {
                changed = true;
            }
            // If [mcp_servers] table is now empty, drop it entirely so we don't
            // leave a stray empty header.
            if servers.is_empty() {
                root.remove("mcp_servers");
            }
        }
    }

    if !changed {
        println!(
            "— glance not registered in codex ({}), nothing to remove",
            path.display()
        );
        return Ok(());
    }

    let serialized = toml::to_string_pretty(&doc).context("serialize codex config")?;
    std::fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;
    println!("\u{2713} removed glance from codex ({})", path.display());
    Ok(())
}

/// Used by `glance doctor` — does the codex config currently register glance?
pub fn is_registered() -> Result<bool> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path)?;
    let doc: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let registered = doc
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("glance"))
        .is_some();
    Ok(registered)
}
