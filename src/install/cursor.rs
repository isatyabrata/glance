//! Cursor registration: `~/.cursor/mcp.json`, `mcpServers.glance`.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

use super::{backup_if_exists, home_join};

pub fn config_path() -> Result<PathBuf> {
    home_join(".cursor/mcp.json")
}

pub fn install() -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    backup_if_exists(&path)?;

    let mut doc: Value = if path.exists() {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if raw.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
        }
    } else {
        Value::Object(serde_json::Map::new())
    };

    let root = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("cursor config is not a JSON object"))?;

    let entry = root
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let servers = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers is not an object"))?;

    servers.insert(
        "glance".to_string(),
        json!({
            "command": "glance-mcp",
            "args": [],
            "env": {}
        }),
    );

    let serialized = serde_json::to_string_pretty(&doc).context("serialize cursor config")?;
    std::fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;

    println!("\u{2713} registered glance to cursor ({})", path.display());
    Ok(())
}

pub fn remove() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!(
            "— cursor config not found ({}), nothing to remove",
            path.display()
        );
        return Ok(());
    }
    backup_if_exists(&path)?;

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if raw.trim().is_empty() {
        println!(
            "— cursor config is empty ({}), nothing to remove",
            path.display()
        );
        return Ok(());
    }
    let mut doc: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;

    let mut changed = false;
    if let Some(root) = doc.as_object_mut() {
        if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
            if servers.remove("glance").is_some() {
                changed = true;
            }
            if servers.is_empty() {
                root.remove("mcpServers");
            }
        }
    }

    if !changed {
        println!(
            "— glance not registered in cursor ({}), nothing to remove",
            path.display()
        );
        return Ok(());
    }

    let serialized = serde_json::to_string_pretty(&doc).context("serialize cursor config")?;
    std::fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;
    println!("\u{2713} removed glance from cursor ({})", path.display());
    Ok(())
}

pub fn is_registered() -> Result<bool> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let doc: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let registered = doc
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("glance"))
        .is_some();
    Ok(registered)
}
