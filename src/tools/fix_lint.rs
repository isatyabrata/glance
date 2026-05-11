//! `fix_lint` — propose a lint-cleanup of a source file. The sub-agent reads
//! the target, identifies low-risk lint issues (unused imports, formatting,
//! naming) and emits the full fixed file as a patch under
//! `~/.glance/patches/<ts>-fix_lint-<basename>.patch`. Patch mode = `overwrite`.
//!
//! Like the other write tools, glance never modifies the target itself.

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
    linter: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "fix_lint".into(),
        description:
            "Propose a lint-fix rewrite of a source file. Fixes only safe issues — unused \
             imports, formatting, naming — never changes behavior. Output is written as a \
             patch under `~/.glance/patches/`; glance does not modify the target itself. \
             `linter` may be `ruff`, `eslint`, `clippy`, `prettier`, or `auto` (default)."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target_file": { "type": "string", "description": "Source file (absolute or cwd-relative)." },
                "linter": {
                    "type": "string",
                    "enum": ["ruff", "eslint", "clippy", "prettier", "auto"],
                    "description": "Default `auto` — picked from file extension."
                }
            },
            "required": ["target_file"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        target_file,
        linter,
    } = serde_json::from_value(args)?;
    let target_abs = resolve_path(&target_file)?;
    let linter = linter.unwrap_or_else(|| "auto".to_string());
    if !matches!(
        linter.as_str(),
        "ruff" | "eslint" | "clippy" | "prettier" | "auto"
    ) {
        return Ok(CallToolResult::error(format!(
            "invalid linter `{}`; expected ruff|eslint|clippy|prettier|auto",
            linter
        )));
    }

    if tokio::fs::metadata(&target_abs).await.is_err() {
        return Ok(CallToolResult::error(format!(
            "target_file does not exist: {}",
            target_abs.display()
        )));
    }

    let lang = detect_lang(&target_abs);
    let chosen_linter = if linter == "auto" {
        default_linter(lang).to_string()
    } else {
        linter.clone()
    };

    let system = format!(
        "You are glance's lint-fix sub-agent. Read the target file with read_file, then output \
         the FULL fixed file content. Behavior must be unchanged.\n\n\
         Allowed fixes:\n\
         - Remove unused imports / variables (only when truly unreferenced).\n\
         - Apply standard formatting for the language (indentation, trailing commas, line \
           endings, quote style if it's the project default).\n\
         - Fix obvious naming issues (e.g. unused-prefix `_`, snake_case vs camelCase \
           mismatches that the linter would flag).\n\
         - Drop redundant `as` / `clone()` / parens that the linter flags as unneeded.\n\n\
         Forbidden:\n\
         - Refactoring control flow.\n\
         - Renaming public symbols.\n\
         - Changing types or signatures.\n\n\
         Before the fixed file content, output a single line of the form:\n\
         `LINT_SUMMARY: <short categorized list>`\n\
         e.g. `LINT_SUMMARY: removed 3 unused imports, fixed 2 formatting issues`.\n\
         Then a single line with `---` and then the full file content. No markdown fences.\n\n\
         Target language: {lang}\n\
         Linter: {linter}\n\
         Target file: {path}",
        lang = lang.as_str(),
        linter = chosen_linter,
        path = target_abs.display(),
    );

    let user = format!(
        "Fix lint issues in `{}`. Output the LINT_SUMMARY line, then `---`, then the full \
         fixed file content.",
        target_abs.display()
    );

    let run = sub_agent::run(&system, &user).await?;
    tracing::info!(
        linter = %chosen_linter,
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        "fix_lint sub_agent done"
    );

    let raw = strip_code_fences(&run.answer);
    let (summary_line, content_str) = split_summary(&raw);

    let mut content = content_str.to_string();
    if content.trim().is_empty() {
        return Ok(CallToolResult::error(
            "sub_agent returned empty file content — nothing to write".to_string(),
        ));
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }

    let reason = if summary_line.trim().is_empty() {
        format!(
            "lint fixes for {} ({})",
            target_abs.display(),
            chosen_linter
        )
    } else {
        format!("{} ({})", summary_line.trim(), chosen_linter)
    };

    let patch_path = write_patch("fix_lint", &target_abs, "overwrite", &reason, &content).await?;

    let summary_json = json!({
        "patch": patch_path.display().to_string(),
        "target": target_abs.display().to_string(),
        "mode": "overwrite",
        "linter": chosen_linter,
        "reason": reason,
        "applied": false,
        "note": "glance does not apply the patch; the MCP caller decides.",
    });
    Ok(CallToolResult::text(serde_json::to_string_pretty(
        &summary_json,
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

fn default_linter(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "clippy",
        Lang::Python => "ruff",
        Lang::JavaScript | Lang::TypeScript => "eslint",
        Lang::Other => "prettier",
    }
}

/// Pull a leading `LINT_SUMMARY: ...` line out of the model's response. If the
/// model didn't follow instructions, return an empty summary and the entire
/// text as content.
fn split_summary(s: &str) -> (String, String) {
    // Look for `LINT_SUMMARY:` on the first non-empty line.
    let mut lines = s.lines();
    let first = match lines.find(|l| !l.trim().is_empty()) {
        Some(l) => l,
        None => return (String::new(), s.to_string()),
    };
    if !first.trim_start().starts_with("LINT_SUMMARY:") {
        return (String::new(), s.to_string());
    }
    let summary = first
        .trim_start()
        .trim_start_matches("LINT_SUMMARY:")
        .trim()
        .to_string();

    // Skip the summary line + an optional `---` separator from the original
    // string, preserving the rest verbatim (including original line endings as
    // best as the line iterator allows).
    let after_summary = match s.split_once(first) {
        Some((_, rest)) => rest.strip_prefix('\n').unwrap_or(rest),
        None => s,
    };
    // Optional separator
    let body = if let Some(rest) = after_summary.trim_start().strip_prefix("---") {
        rest.strip_prefix('\n').unwrap_or(rest).to_string()
    } else {
        after_summary.to_string()
    };
    (summary, body)
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
    fn default_linter_by_lang() {
        assert_eq!(default_linter(Lang::Rust), "clippy");
        assert_eq!(default_linter(Lang::Python), "ruff");
        assert_eq!(default_linter(Lang::TypeScript), "eslint");
        assert_eq!(default_linter(Lang::JavaScript), "eslint");
        assert_eq!(default_linter(Lang::Other), "prettier");
    }

    #[test]
    fn split_summary_extracts_and_strips_separator() {
        let raw = "LINT_SUMMARY: removed 1 unused import\n---\nfn main() {}\n";
        let (summary, body) = split_summary(raw);
        assert_eq!(summary, "removed 1 unused import");
        assert_eq!(body, "fn main() {}\n");
    }

    #[test]
    fn split_summary_missing_returns_full_body() {
        let raw = "fn main() {}\n";
        let (summary, body) = split_summary(raw);
        assert_eq!(summary, "");
        assert_eq!(body, "fn main() {}\n");
    }

    #[test]
    fn split_summary_handles_no_separator() {
        let raw = "LINT_SUMMARY: nothing to do\n\nfn main() {}\n";
        let (summary, body) = split_summary(raw);
        assert_eq!(summary, "nothing to do");
        assert!(body.contains("fn main() {}"));
    }
}
