//! `write_docs` — propose documentation for a source file. Two flavors:
//!
//! - `kind = "docstring"` (default): rewrite the same file with docstrings /
//!   doc comments added to public symbols. Patch mode = `overwrite` on target.
//! - `kind = "readme"`: emit a sibling README.md with high-level docs. Patch
//!   mode = `create` on `<dir>/README.md`.
//!
//! As with `md_write`, glance never touches the target — it only writes a
//! patch under `~/.glance/patches/<ts>-write_docs-<basename>.patch`.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::mcp::{
    protocol::{CallToolResult, ToolDefinition},
    sub_agent,
};

#[derive(Debug, Deserialize)]
struct Args {
    target_file: String,
    #[serde(default)]
    kind: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "write_docs".into(),
        description:
            "Generate documentation for a source file. `kind=docstring` (default) rewrites the \
             same file with doc comments added to public symbols (Python triple-quoted, JS/TS \
             JSDoc, Rust `///`). `kind=readme` emits a sibling README.md with high-level docs. \
             Output goes to a patch under `~/.glance/patches/`; glance does not modify any \
             source itself."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target_file": { "type": "string", "description": "Source file (absolute or cwd-relative)." },
                "kind":        { "type": "string", "enum": ["docstring", "readme"], "description": "Default `docstring`." }
            },
            "required": ["target_file"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { target_file, kind } = serde_json::from_value(args)?;
    let target_abs = resolve_path(&target_file)?;
    let kind = kind.unwrap_or_else(|| "docstring".to_string());
    if !matches!(kind.as_str(), "docstring" | "readme") {
        return Ok(CallToolResult::error(format!(
            "invalid kind `{}`; expected docstring|readme",
            kind
        )));
    }

    if tokio::fs::metadata(&target_abs).await.is_err() {
        return Ok(CallToolResult::error(format!(
            "target_file does not exist: {}",
            target_abs.display()
        )));
    }

    let lang = detect_lang(&target_abs);

    let (system, user, patch_target, patch_mode, reason) = match kind.as_str() {
        "docstring" => {
            let sys = format!(
                "You are glance's documentation sub-agent. Goal: read the target file and \
                 produce the SAME file with doc comments added to every public symbol \
                 (function / class / struct / enum / trait / exported const). \n\n\
                 Conventions by language:\n\
                 - Python: triple-quoted docstrings on the line(s) immediately inside def/class.\n\
                 - JavaScript / TypeScript: JSDoc `/** ... */` directly above the symbol.\n\
                 - Rust: `///` doc comments directly above the symbol; inner `//!` for modules.\n\n\
                 Constraints:\n\
                 1. Do NOT change behavior — preserve all code lines verbatim except for \
                    inserted doc comments.\n\
                 2. Do NOT touch private / internal symbols unless they already had docs.\n\
                 3. Do NOT add markdown fences. Output ONLY the full updated file content. \
                    The first line of your reply must be the first line of the file.\n\n\
                 Target language: {lang}\n\
                 Target file: {path}",
                lang = lang.as_str(),
                path = target_abs.display(),
            );
            let usr = format!(
                "Read `{}` (call read_file). Then output the full updated file with docstrings \
                 added to public symbols.",
                target_abs.display()
            );
            (
                sys,
                usr,
                target_abs.clone(),
                "overwrite",
                format!(
                    "added doc comments to public symbols in {} ({})",
                    target_abs.display(),
                    lang.as_str()
                ),
            )
        }
        "readme" => {
            let parent = target_abs
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            let readme = parent.join("README.md");
            let sys = format!(
                "You are glance's README sub-agent. Read the target file (and 1–2 obvious \
                 sibling files for context if helpful) and produce a README.md that \
                 documents the public API of this module.\n\n\
                 Required sections, in order:\n\
                 - `# <module name>` (one-line summary)\n\
                 - `## Overview`\n\
                 - `## API` (one subsection per public symbol; show the signature)\n\
                 - `## Examples`\n\
                 - `## Errors`\n\n\
                 Output ONLY the markdown content. No code fences around the entire file, \
                 no leading commentary. The first line must be the `# ` heading.\n\n\
                 Target file: {path}",
                path = target_abs.display(),
            );
            let usr = format!(
                "Generate the README.md for `{}`. Sibling target: `{}`.",
                target_abs.display(),
                readme.display()
            );
            (
                sys,
                usr,
                readme.clone(),
                "create",
                format!("README for {}", target_abs.display()),
            )
        }
        _ => unreachable!(),
    };

    // For "create" mode, refuse early if the README already exists.
    if patch_mode == "create" && tokio::fs::metadata(&patch_target).await.is_ok() {
        return Ok(CallToolResult::error(format!(
            "create mode but target already exists: {}",
            patch_target.display()
        )));
    }

    let run = sub_agent::run(&system, &user).await?;
    tracing::info!(
        kind = %kind,
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        "write_docs sub_agent done"
    );

    let mut content = strip_code_fences(&run.answer);
    if content.trim().is_empty() {
        return Ok(CallToolResult::error(
            "sub_agent returned empty content — nothing to write".to_string(),
        ));
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let patch_path =
        write_patch("write_docs", &patch_target, patch_mode, &reason, &content).await?;

    let summary = json!({
        "patch": patch_path.display().to_string(),
        "target": patch_target.display().to_string(),
        "mode": patch_mode,
        "kind": kind,
        "applied": false,
        "note": "glance does not apply the patch; the MCP caller decides.",
    });
    Ok(CallToolResult::text(serde_json::to_string_pretty(
        &summary,
    )?))
}

#[derive(Clone, Copy, Debug)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Other,
}

impl Lang {
    fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Other => "other",
        }
    }
}

fn detect_lang(p: &Path) -> Lang {
    match p.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "rs" => Lang::Rust,
        "py" => Lang::Python,
        "ts" | "tsx" => Lang::TypeScript,
        "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
        _ => Lang::Other,
    }
}

fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim_start_matches('\u{feff}');
    let lines: Vec<&str> = trimmed.lines().collect();
    let first_idx = lines.iter().position(|l| !l.trim().is_empty());
    let last_idx = lines.iter().rposition(|l| !l.trim().is_empty());
    if let (Some(fi), Some(li)) = (first_idx, last_idx) {
        if fi < li && lines[fi].trim_start().starts_with("```") && lines[li].trim() == "```" {
            return lines[fi + 1..li].join("\n");
        }
    }
    trimmed.to_string()
}

fn resolve_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn patches_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("patches"))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

async fn write_patch(
    tool: &str,
    target: &Path,
    mode: &str,
    reason: &str,
    content: &str,
) -> Result<PathBuf> {
    let dir = patches_dir()?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("mkdir -p {}", dir.display()))?;

    let basename = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into());
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let patch_name = format!("{}-{}-{}.patch", ts, tool, sanitize(&basename));
    let patch_path = dir.join(&patch_name);

    let header = json!({
        "target": target.display().to_string(),
        "mode": mode,
        "reason": reason,
    });

    let mut body = String::new();
    body.push_str(&serde_json::to_string(&header)?);
    body.push('\n');
    body.push_str("---\n");
    body.push_str(content);
    if !content.ends_with('\n') {
        body.push('\n');
    }

    tokio::fs::write(&patch_path, body.as_bytes())
        .await
        .with_context(|| format!("write {}", patch_path.display()))?;
    Ok(patch_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_lang_smoke() {
        assert!(matches!(detect_lang(Path::new("x.rs")), Lang::Rust));
        assert!(matches!(detect_lang(Path::new("x.py")), Lang::Python));
        assert!(matches!(detect_lang(Path::new("x.tsx")), Lang::TypeScript));
        assert!(matches!(detect_lang(Path::new("x.mjs")), Lang::JavaScript));
        assert!(matches!(detect_lang(Path::new("x.zig")), Lang::Other));
    }

    #[test]
    fn strip_outer_fences() {
        let s = "```md\n# Title\nhi\n```";
        assert_eq!(strip_code_fences(s), "# Title\nhi");
    }

    #[test]
    fn no_fences_passthrough() {
        let s = "# Title\nhi\n";
        assert_eq!(strip_code_fences(s), "# Title\nhi\n");
    }
}
