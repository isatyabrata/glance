//! `explain` — point-and-explain for one or more file targets.
//!
//! The MCP client gives us either a single `target` (back-compat) or a
//! `targets[]` array. With multiple targets the sub-agent runs ONCE with all
//! files visible, sharing system prompt + setup overhead — that's typically a
//! 2-3× speedup vs calling `explain` N times for adjacent files. Output is a
//! single text block with `### <target>` section headers when batched.

use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::{
    protocol::{CallToolResult, ToolDefinition},
    sub_agent,
};

/// Strip an optional `:line` or `:line:col` suffix from a target spec,
/// then check whether the resulting filesystem path exists.
fn path_exists_strip_locator(spec: &str) -> bool {
    // Walk back from the end and trim trailing `:N` segments while they are
    // pure digits — handles `path:42`, `path:42:7`, but leaves `c:\foo` and
    // `http://x` alone (the trimmed segment must be all-digit).
    let mut s: &str = spec;
    for _ in 0..2 {
        if let Some((head, tail)) = s.rsplit_once(':') {
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                s = head;
                continue;
            }
        }
        break;
    }
    Path::new(s).exists()
}

#[derive(Debug, Deserialize)]
struct Args {
    /// Single file/location target — kept for backward compatibility.
    #[serde(default)]
    target: Option<String>,
    /// Multiple file/location targets in one call. Preferred when explaining
    /// 2+ adjacent files: one sub-agent loop instead of N.
    #[serde(default)]
    targets: Option<Vec<String>>,
    /// What the caller wants explained. Defaults to a generic 2-paragraph ask.
    #[serde(default)]
    question: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "explain".into(),
        description: "Explain what a specific file/location does, in prose. Pass a file path or \
             `path:line_number` as `target`, OR an array of paths as `targets` to explain \
             several files in ONE sub-agent loop (2-3× faster than N separate calls). Returns \
             at most 3 short paragraphs per target — never raw code. Use this instead of \
             reading the file(s) yourself when you just need to understand them."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Single file path, or `path:line_number` to point at a specific spot.",
                },
                "targets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Multiple file paths (or `path:line` entries). When you have 2+ adjacent files to explain, prefer this over calling `explain` N times — saves system-prompt and round-trip overhead.",
                },
                "question": {
                    "type": "string",
                    "description": "What you want explained. Default: 'explain what this code does, in 2 paragraphs.'",
                },
            },
            "oneOf": [
                { "required": ["target"] },
                { "required": ["targets"] }
            ],
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        target,
        targets,
        question,
    } = serde_json::from_value(args)?;

    // Normalize: targets array wins if non-empty; single target falls back.
    let resolved: Vec<String> = match (targets, target) {
        (Some(ts), _) if !ts.is_empty() => ts,
        (_, Some(t)) => vec![t],
        _ => {
            return Ok(CallToolResult::error(
                "[explain] must pass either `target` (string) or `targets` (string[]).".to_string(),
            ));
        }
    };

    // Pre-validate: strip optional `:line` / `:line:col` suffix, then check
    // each path exists on disk before booting the sub-agent. Otherwise the
    // model burns tokens hallucinating an explanation of a non-existent file.
    let missing: Vec<&String> = resolved
        .iter()
        .filter(|t| !path_exists_strip_locator(t))
        .collect();
    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|p| format!("  - {}", p))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(CallToolResult::error(format!(
            "[explain] file not found ({} of {}):\n{}\n\nCheck the path. Use list_dir / glob \
             via `glance.search` first if you're not sure what exists.",
            missing.len(),
            resolved.len(),
            list
        )));
    }

    let question =
        question.unwrap_or_else(|| "explain what this code does, in 2 paragraphs.".to_string());

    let (system, user) = if resolved.len() == 1 {
        (
            "You are glance's `explain` sub-agent. The caller pointed you at a specific \
             file/location and asked a question. Read it, optionally read its tight neighbors \
             (only files it imports/calls into IF needed), and answer in at most 3 short \
             paragraphs. Never dump raw code back. Cite file:line when referencing things."
                .to_string(),
            format!("Target: {}\nQuestion: {}", resolved[0], question),
        )
    } else {
        let n = resolved.len();
        (
            format!(
                "You are glance's `explain` sub-agent. The caller gave you {n} file targets \
                 and ONE question to apply to each. Read every target (use read_file with \
                 offset/limit for large files; grep first if a target points at a symbol). \
                 Then write the answer as a single markdown block with `### <target>` section \
                 headers, one per target, each section AT MOST 2 short paragraphs. No raw \
                 code dumps. Cite file:line where useful. Order sections in input order."
            ),
            format!(
                "Targets ({} files):\n{}\n\nQuestion (apply to each): {}",
                n,
                resolved
                    .iter()
                    .map(|t| format!("- {}", t))
                    .collect::<Vec<_>>()
                    .join("\n"),
                question
            ),
        )
    };

    let run = sub_agent::run(&system, &user).await?;
    tracing::info!(
        targets = resolved.len(),
        iters = run.iterations,
        tool_calls = run.tool_calls,
        tokens = run.usage_total.total_tokens,
        "explain completed"
    );
    Ok(CallToolResult::text(run.answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_neither_target_nor_targets() {
        let v = json!({ "question": "what is this" });
        let args: Args = serde_json::from_value(v).unwrap();
        assert!(args.target.is_none() && args.targets.is_none());
    }

    #[test]
    fn parses_targets_array() {
        let v = json!({ "targets": ["a.rs", "b.rs"] });
        let args: Args = serde_json::from_value(v).unwrap();
        assert_eq!(args.targets.unwrap().len(), 2);
        assert!(args.target.is_none());
    }

    #[test]
    fn parses_single_target_back_compat() {
        let v = json!({ "target": "src/lib.rs" });
        let args: Args = serde_json::from_value(v).unwrap();
        assert_eq!(args.target.as_deref(), Some("src/lib.rs"));
        assert!(args.targets.is_none());
    }

    #[test]
    fn strip_locator_handles_line_and_col() {
        // Compare what `path_exists_strip_locator` would *check* — we can't
        // depend on the actual filesystem in tests, but we can confirm the
        // suffix-stripping is sane by checking against `Cargo.toml` which the
        // crate root always has, plus a path we know never exists.
        assert!(super::path_exists_strip_locator("Cargo.toml"));
        assert!(super::path_exists_strip_locator("Cargo.toml:42"));
        assert!(super::path_exists_strip_locator("Cargo.toml:42:7"));
        assert!(!super::path_exists_strip_locator(
            "src/definitely_not_a_real_file.rs"
        ));
        assert!(!super::path_exists_strip_locator(
            "src/definitely_not_a_real_file.rs:99"
        ));
    }

    #[test]
    fn strip_locator_leaves_non_digit_suffix_alone() {
        // Don't trim windows drive letters or URL schemes — only all-digit
        // trailing segments after a colon.
        assert!(!super::path_exists_strip_locator("c:/no_such_dir/no_such"));
        assert!(!super::path_exists_strip_locator("http://example.com"));
    }
}
