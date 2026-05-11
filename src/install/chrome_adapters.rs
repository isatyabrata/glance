//! YAML adapter framework.
//!
//! Adapters are user-defined "site recipes" that bundle a JS expression with
//! parameters and an optional URL match hint, so frequent workflows don't
//! force the LLM to re-discover selectors / API endpoints every time.
//!
//! File layout: one YAML per adapter at `~/.glance/chrome-adapters/<name>.yaml`.
//!
//! Schema:
//!
//! ```yaml
//! name: oms_today_sales
//! description: Today's OMS variant sales summary (Prints10).
//! match_url: '^https://2\.innerchic\.cn/variant-sales'   # optional
//! await_promise: true                                    # default true
//! args:
//!   - name: date
//!     description: 'YYYY-MM-DD; defaults to today.'
//!     required: false
//! evaluate: |
//!   (async () => {
//!     // ...
//!     return { /* shape the JSON the LLM gets back */ };
//!   })()
//! ```
//!
//! At runtime, `chrome run_adapter {name, args, tab_id?}` looks the adapter
//! up, picks a tab (caller-supplied OR first matching `match_url`), prepends
//! `const args = {...};` to the script, and forwards to `tabs.evaluate`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Where users keep their adapter YAMLs. Created on first save.
pub fn adapters_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    Ok(home.join(".glance/chrome-adapters"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterArg {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adapter {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub match_url: Option<String>,
    #[serde(default)]
    pub args: Vec<AdapterArg>,
    pub evaluate: String,
    #[serde(default = "default_true")]
    pub await_promise: bool,
    /// Execution world: "main" (default) or "cdp". CDP bypasses page CSP — set
    /// to "cdp" for adapters targeting sites like X.com that block `unsafe-eval`.
    #[serde(default)]
    pub world: Option<String>,
    /// Set by the loader, not the YAML — absolute path on disk for round-trip.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

/// Load every `*.yaml` / `*.yml` from the adapters dir. Returns a map keyed
/// by adapter name. Malformed files are skipped with a `tracing::warn!`.
pub fn load_all() -> Result<BTreeMap<String, Adapter>> {
    let dir = adapters_dir()?;
    let mut out = BTreeMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !is_yaml(&path) {
            continue;
        }
        match load_one(&path) {
            Ok(mut a) => {
                a.source_path = Some(path.clone());
                if out.contains_key(&a.name) {
                    tracing::warn!(name = %a.name, path = %path.display(), "duplicate adapter name; later file wins");
                }
                out.insert(a.name.clone(), a);
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "skipping malformed adapter");
            }
        }
    }
    Ok(out)
}

fn is_yaml(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "yaml" || e == "yml")
        .unwrap_or(false)
}

fn load_one(path: &Path) -> Result<Adapter> {
    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut a: Adapter = serde_yaml::from_str(&body)
        .with_context(|| format!("parse YAML {}", path.display()))?;
    // Validation
    if a.name.trim().is_empty() {
        return Err(anyhow!("{}: adapter `name` is required", path.display()));
    }
    if a.evaluate.trim().is_empty() {
        return Err(anyhow!("{}: adapter `evaluate` is required", path.display()));
    }
    a.name = a.name.trim().to_string();
    Ok(a)
}

/// Persist an adapter to disk. Uses `<name>.yaml`; overwrites existing.
pub fn save(adapter: &Adapter) -> Result<PathBuf> {
    let dir = adapters_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.yaml", sanitize(&adapter.name)));
    let body = serde_yaml::to_string(adapter)?;
    fs::write(&path, body)?;
    Ok(path)
}

/// Save raw YAML text — used by the GUI editor where the user edits source
/// directly. We still parse to validate before committing.
pub fn save_raw(name: &str, yaml_body: &str) -> Result<PathBuf> {
    let mut a: Adapter = serde_yaml::from_str(yaml_body)
        .with_context(|| "parse YAML before save")?;
    if a.name.trim().is_empty() {
        a.name = name.to_string();
    }
    if a.name != name {
        return Err(anyhow!(
            "name in YAML ({}) doesn't match save target ({})",
            a.name,
            name
        ));
    }
    let dir = adapters_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.yaml", sanitize(name)));
    fs::write(&path, yaml_body)?;
    Ok(path)
}

pub fn delete(name: &str) -> Result<()> {
    let path = adapters_dir()?.join(format!("{}.yaml", sanitize(name)));
    if path.exists() {
        fs::remove_file(&path)?;
    } else {
        let alt = adapters_dir()?.join(format!("{}.yml", sanitize(name)));
        if alt.exists() {
            fs::remove_file(&alt)?;
        }
    }
    Ok(())
}

pub fn read_raw(name: &str) -> Result<String> {
    let dir = adapters_dir()?;
    let p1 = dir.join(format!("{}.yaml", sanitize(name)));
    if p1.exists() {
        return Ok(fs::read_to_string(p1)?);
    }
    let p2 = dir.join(format!("{}.yml", sanitize(name)));
    if p2.exists() {
        return Ok(fs::read_to_string(p2)?);
    }
    Err(anyhow!("adapter not found: {}", name))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Validate a user-supplied adapter `name`: 1-32 chars from `[a-z0-9_-]`.
/// Returns `Ok(())` on success or a human-readable error.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("adapter name cannot be empty"));
    }
    if name.len() > 32 {
        return Err(anyhow!("adapter name too long (max 32 chars)"));
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !ok {
            return Err(anyhow!(
                "adapter name `{}` contains invalid char `{}` (allowed: a-z 0-9 _ -)",
                name,
                c
            ));
        }
    }
    Ok(())
}

/// Derive a permissive `match_url` regex from a captured URL by anchoring on
/// scheme + host. e.g. `https://shop.example.com/foo?bar=1` →
/// `^https://shop\.example\.com/`. Falls back to a literal-prefix regex if the
/// URL fails to parse.
pub fn derive_match_url_from(url_str: &str) -> String {
    if let Ok(u) = url::Url::parse(url_str) {
        if let Some(host) = u.host_str() {
            let scheme = u.scheme();
            let escaped_host = regex::escape(host);
            return format!("^{}://{}/", scheme, escaped_host);
        }
    }
    // Fallback: escape the whole thing and anchor at start.
    format!("^{}", regex::escape(url_str))
}

/// Bake the (validated) `args` map into a JS preamble so the adapter's
/// `evaluate` body can read `args.foo`.
pub fn build_invocation_script(adapter: &Adapter, args: &serde_json::Value) -> Result<String> {
    let preamble = format!(
        "const args = {};\n",
        serde_json::to_string(args).unwrap_or_else(|_| "{}".into())
    );
    Ok(format!("(() => {{ {}return ({}); }})()", preamble, adapter.evaluate))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_script_wraps_args() {
        let a = Adapter {
            name: "x".into(),
            description: None,
            match_url: None,
            args: vec![],
            evaluate: "args.n + 1".into(),
            await_promise: true,
            world: None,
            source_path: None,
        };
        let s = build_invocation_script(&a, &serde_json::json!({ "n": 41 })).unwrap();
        assert!(s.contains("const args = {\"n\":41};"));
        assert!(s.contains("args.n + 1"));
    }

    #[test]
    fn validate_name_accepts_lowercase_alnum_and_separators() {
        assert!(validate_name("ok").is_ok());
        assert!(validate_name("ok_one-2").is_ok());
        assert!(validate_name(&"a".repeat(32)).is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(33)).is_err());
        assert!(validate_name("BadCase").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("dot.case").is_err());
    }

    #[test]
    fn derive_match_url_anchors_origin() {
        let s = derive_match_url_from("https://shop.example.com/foo?bar=1");
        assert_eq!(s, "^https://shop\\.example\\.com/");
        let s2 = derive_match_url_from("http://localhost:8080/x");
        assert!(s2.starts_with("^http://localhost"), "got {}", s2);
    }
}
