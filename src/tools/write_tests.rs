//! `write_tests` — generate a unit-test file (or inline test module) for a
//! source file via the sub-agent, and emit the result as a patch under
//! `~/.glance/patches/`. glance never modifies the target itself.
//!
//! Patch shape mirrors `md_write`:
//!
//! ```text
//! {"target":"/abs/path","mode":"create","reason":"..."}
//! ---
//! <full file content>
//! ```

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
    framework: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "write_tests".into(),
        description:
            "Generate unit tests for a source file. Drives a sub-agent that may read sibling \
             test files for conventions, then emits the proposed test code as a patch under \
             `~/.glance/patches/<ts>-write_tests-<basename>.patch`. glance does NOT touch the \
             target. The MCP caller decides whether to apply. For Rust, the patch appends an \
             inline `#[cfg(test)] mod tests` block to the same file; for Python/JS/TS it creates \
             a sibling test file."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target_file":  { "type": "string", "description": "Source file to test (absolute or cwd-relative)." },
                "framework":    { "type": "string", "description": "Optional. e.g. pytest / jest / cargo-test. Inferred if absent." },
                "instructions": { "type": "string", "description": "Optional user hints for what to cover." }
            },
            "required": ["target_file"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        target_file,
        framework,
        instructions,
    } = serde_json::from_value(args)?;
    let target_abs = resolve_path(&target_file)?;

    if tokio::fs::metadata(&target_abs).await.is_err() {
        return Ok(CallToolResult::error(format!(
            "target_file does not exist: {}",
            target_abs.display()
        )));
    }

    let lang = detect_lang(&target_abs);
    let (test_target, mode) = test_target_for(&target_abs, lang);
    let inferred_framework = framework
        .clone()
        .unwrap_or_else(|| default_framework(lang).to_string());

    let system = format!(
        "You are glance's test-writing sub-agent. Your job: produce ONLY the raw file content \
         that should be written as a unit test. No explanation, no markdown fences, no leading \
         commentary — the very first line of your reply must be the first line of the test file. \
         \n\nProcedure:\n\
         1. Call read_file on the target source file: {target}\n\
         2. Use grep / list_dir to find 1–2 sibling test files (look for names containing `test`) \
            and read them to match this codebase's conventions.\n\
         3. Output the COMPLETE file content for the test file. For Rust inline tests, output \
            the *full updated source file* (original code + an appended `#[cfg(test)] mod tests \
            {{ ... }}` block). For Python/JS/TS, output a standalone test file.\n\n\
         Framework hint: {framework}\n\
         File extension hint: {ext}\n\
         Mode: {mode}\n",
        target = target_abs.display(),
        framework = inferred_framework,
        ext = lang.as_str(),
        mode = mode,
    );

    let user_hints = instructions
        .clone()
        .unwrap_or_else(|| "(no extra instructions)".to_string());
    let user = format!("Write tests for `{}`. {}", target_abs.display(), user_hints);

    let run = sub_agent::run(&system, &user).await?;
    tracing::info!(
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        "write_tests sub_agent done"
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

    // For Rust inline-tests we output the full updated file (overwrite). Map
    // our internal "append" intent to "overwrite" so the patch body is a
    // self-contained replacement and the user sees the whole new file.
    let patch_mode = match mode {
        "append-rust" => "overwrite",
        m => m,
    };

    let reason = format!(
        "auto-generated tests for {} ({}; framework={})",
        target_abs.display(),
        lang.as_str(),
        inferred_framework,
    );

    let patch_path =
        write_patch("write_tests", &test_target, patch_mode, &reason, &content).await?;

    let summary = json!({
        "patch": patch_path.display().to_string(),
        "target": test_target.display().to_string(),
        "mode": patch_mode,
        "framework": inferred_framework,
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

fn default_framework(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "cargo-test",
        Lang::Python => "pytest",
        Lang::JavaScript | Lang::TypeScript => "jest",
        Lang::Other => "auto",
    }
}

/// Decide the test file path and conceptual mode for the language.
/// Returns (target_path, mode_tag). Mode `"append-rust"` is internal — it gets
/// rewritten to `"overwrite"` for the patch header (we ask the model for the
/// full updated file).
fn test_target_for(src: &Path, lang: Lang) -> (PathBuf, &'static str) {
    let parent = src.parent().unwrap_or(Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("module");
    match lang {
        Lang::Rust => (src.to_path_buf(), "append-rust"),
        Lang::Python => (parent.join(format!("test_{}.py", stem)), "create"),
        Lang::TypeScript => {
            let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("ts");
            let test_ext = if ext == "tsx" { "test.tsx" } else { "test.ts" };
            (parent.join(format!("{}.{}", stem, test_ext)), "create")
        }
        Lang::JavaScript => {
            let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("js");
            let test_ext = if ext == "jsx" { "test.jsx" } else { "test.js" };
            (parent.join(format!("{}.{}", stem, test_ext)), "create")
        }
        Lang::Other => (parent.join(format!("{}.test", stem)), "create"),
    }
}

fn strip_code_fences(s: &str) -> String {
    // Strip an outer ```lang ... ``` if the model leaked one in despite the
    // instructions. Conservative: only strip when the very first non-empty
    // line is a fence and the very last non-empty line is a fence.
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

/// Write a patch file in the same JSON-header-then-content format as md_write.
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
    use std::path::PathBuf;

    #[test]
    fn detect_lang_by_extension() {
        assert!(matches!(detect_lang(Path::new("a.rs")), Lang::Rust));
        assert!(matches!(detect_lang(Path::new("a.py")), Lang::Python));
        assert!(matches!(detect_lang(Path::new("a.ts")), Lang::TypeScript));
        assert!(matches!(detect_lang(Path::new("a.tsx")), Lang::TypeScript));
        assert!(matches!(detect_lang(Path::new("a.js")), Lang::JavaScript));
        assert!(matches!(detect_lang(Path::new("a.txt")), Lang::Other));
    }

    #[test]
    fn test_target_python_creates_sibling() {
        let (p, mode) = test_target_for(Path::new("/tmp/foo.py"), Lang::Python);
        assert_eq!(p, PathBuf::from("/tmp/test_foo.py"));
        assert_eq!(mode, "create");
    }

    #[test]
    fn test_target_ts_creates_dot_test() {
        let (p, mode) = test_target_for(Path::new("/tmp/foo.ts"), Lang::TypeScript);
        assert_eq!(p, PathBuf::from("/tmp/foo.test.ts"));
        assert_eq!(mode, "create");
    }

    #[test]
    fn test_target_rust_inline_appends_to_same_file() {
        let (p, mode) = test_target_for(Path::new("/tmp/lib.rs"), Lang::Rust);
        assert_eq!(p, PathBuf::from("/tmp/lib.rs"));
        assert_eq!(mode, "append-rust");
    }

    #[test]
    fn strip_fences_handles_outer_fence() {
        let s = "```rust\nfn x() {}\n```";
        assert_eq!(strip_code_fences(s), "fn x() {}");
    }

    #[test]
    fn strip_fences_passes_through_when_no_outer_fence() {
        let s = "fn x() {}\n";
        assert_eq!(strip_code_fences(s), "fn x() {}\n");
    }
}
