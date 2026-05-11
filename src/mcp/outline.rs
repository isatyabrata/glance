//! Compressed views of source files for the sub-agent's `read_file` tool.
//!
//! The backend model burns tokens proportional to the bytes we feed it. For
//! orientation passes ("what's in this file?") we can usually return only
//! signatures + imports and skip every function body. This module contains
//! per-language line scanners that produce that compressed view.
//!
//! Algorithms (not code) inspired by the rtk project's outline mode. We keep
//! everything regex/line-based — no real parsing — so it stays cheap and
//! works on partially-broken files.
//!
//! Modes:
//! - `outline`  → signatures + imports + (for python) one-line docstrings.
//! - `skeleton` → outline minus docstrings, even tighter.
//!
//! Public entry point is [`render`].

/// Compression mode requested by the sub-agent model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Outline,
    Skeleton,
}

impl Mode {
    pub fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("full").to_ascii_lowercase().as_str() {
            "outline" => Mode::Outline,
            "skeleton" => Mode::Skeleton,
            _ => Mode::Full,
        }
    }
}

/// Render `content` for `path` under the requested compression mode. Returns
/// `None` when the language has no scanner — caller should fall back.
pub fn render(path: &std::path::Path, content: &str, mode: Mode) -> Option<String> {
    if mode == Mode::Full {
        return None;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let kept: Vec<String> = match ext.as_str() {
        "py" => scan_python(&lines, mode),
        "rs" => scan_rust(&lines, mode),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => scan_ts(&lines, mode),
        _ => fallback(&lines),
    };
    Some(kept.join("\n"))
}

/// Compose the outline body and a one-line savings header. `original_lines`
/// is the line count of the underlying file, `kept_body` is what `render`
/// returned. Caller writes both into the tool reply.
pub fn savings_header(mode: Mode, original_lines: usize, kept_body: &str) -> String {
    let kept_lines = if kept_body.is_empty() {
        0
    } else {
        kept_body.lines().count()
    };
    let pct = (original_lines.saturating_sub(kept_lines) * 100)
        .checked_div(original_lines)
        .map_or(0, |v| v.min(99));
    let label = match mode {
        Mode::Outline => "outline mode",
        Mode::Skeleton => "skeleton mode",
        Mode::Full => "full",
    };
    format!(
        "[{}: {} lines compressed to {} lines, ~{}% smaller]",
        label, original_lines, kept_lines, pct
    )
}

// ── Python ──────────────────────────────────────────────────────────────────

fn scan_python(lines: &[&str], mode: Mode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        let indent = raw.len() - trimmed.len();

        // Imports / top-level assignments at column 0 → keep verbatim.
        if indent == 0
            && (trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || (is_top_level_assignment(trimmed)))
        {
            out.push(raw.to_string());
            i += 1;
            continue;
        }

        let is_def = trimmed.starts_with("def ") || trimmed.starts_with("async def ");
        let is_class = trimmed.starts_with("class ");
        if is_def || is_class {
            // Capture signature line(s) until ':'.
            let mut sig = String::new();
            let mut j = i;
            while j < lines.len() {
                sig.push_str(lines[j]);
                if lines[j].contains(':') {
                    break;
                }
                sig.push('\n');
                j += 1;
            }
            out.push(sig);

            // Optional one-line docstring on the next non-blank line at deeper indent.
            let mut k = j + 1;
            while k < lines.len() && lines[k].trim().is_empty() {
                k += 1;
            }
            if mode == Mode::Outline && k < lines.len() {
                let dline = lines[k];
                let dt = dline.trim_start();
                if dt.starts_with("\"\"\"") || dt.starts_with("'''") {
                    let q = if dt.starts_with("\"\"\"") {
                        "\"\"\""
                    } else {
                        "'''"
                    };
                    // Single-line docstring """xyz""" — keep as-is.
                    if dt.len() >= 6 && dt[3..].contains(q) {
                        out.push(dline.to_string());
                    } else if let Some(first) = dt.strip_prefix(q) {
                        // Multi-line docstring — keep just the opener summary line.
                        let summary = first.trim();
                        let pad = &dline[..dline.len() - dt.len()];
                        out.push(format!("{}{}{}{}", pad, q, summary, q));
                    }
                }
            }

            if is_class {
                // Don't skip the class body — keep nested method signatures by
                // letting the outer loop walk into the indented region.
                i = j + 1;
            } else {
                // Skip function body: anything indented deeper than the def.
                i = j + 1;
                while i < lines.len() {
                    let ln = lines[i];
                    if ln.trim().is_empty() {
                        i += 1;
                        continue;
                    }
                    let ln_indent = ln.len() - ln.trim_start().len();
                    if ln_indent <= indent {
                        break;
                    }
                    i += 1;
                }
            }
            continue;
        }

        i += 1;
    }
    out
}

fn is_top_level_assignment(line: &str) -> bool {
    // crude: NAME = … (uppercase-ish or snake_case identifier on the left).
    let Some(eq) = line.find('=') else {
        return false;
    };
    if line.starts_with('#') {
        return false;
    }
    let lhs = line[..eq].trim();
    !lhs.is_empty()
        && lhs
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ',' || c.is_whitespace())
}

// ── Rust ────────────────────────────────────────────────────────────────────

fn scan_rust(lines: &[&str], _mode: Mode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();

        if trimmed.starts_with("use ") || trimmed.starts_with("pub use ") {
            out.push(raw.to_string());
            i += 1;
            continue;
        }

        // fn signatures (incl. pub fn / async fn / pub(crate) fn).
        if rust_is_fn(trimmed) {
            // Signature ends at the line whose open-brace count ≥ 1 OR ';' (trait fn).
            let mut sig = String::new();
            let mut j = i;
            while j < lines.len() {
                sig.push_str(lines[j]);
                if lines[j].contains('{') || lines[j].trim_end().ends_with(';') {
                    break;
                }
                sig.push('\n');
                j += 1;
            }
            // Trim body off: replace anything from '{' onward with '{ … }'.
            if let Some(brace) = sig.find('{') {
                let head = sig[..brace].trim_end().to_string();
                out.push(format!("{} {{ … }}", head));
                i = skip_brace_block(lines, j);
            } else {
                out.push(sig);
                i = j + 1;
            }
            continue;
        }

        // struct / enum / trait / impl headers.
        if rust_is_decl(trimmed) {
            // Keep through the matching '{' line, then drop body for impl/trait;
            // for struct, keep field list (it's usually compact and load-bearing).
            let kind = trimmed.split_whitespace().next().unwrap_or("");
            if trimmed.contains(';') {
                out.push(raw.to_string());
                i += 1;
                continue;
            }
            // Locate the opening brace.
            let mut j = i;
            while j < lines.len() && !lines[j].contains('{') {
                j += 1;
            }
            if j == lines.len() {
                out.push(raw.to_string());
                i = j;
                continue;
            }
            if kind == "struct"
                || kind == "enum"
                || trimmed.contains("pub struct")
                || trimmed.contains("pub enum")
            {
                // Keep the whole declaration including fields/variants.
                let mut depth = 0i32;
                let mut k = i;
                while k < lines.len() {
                    out.push(lines[k].to_string());
                    for c in lines[k].chars() {
                        if c == '{' {
                            depth += 1;
                        } else if c == '}' {
                            depth -= 1;
                        }
                    }
                    k += 1;
                    if depth <= 0 && k > j {
                        break;
                    }
                }
                i = k;
            } else {
                // impl / trait / mod — keep header line only, drop body.
                let head_lines = &lines[i..=j];
                let joined = head_lines.join("\n");
                let head = joined
                    .split('{')
                    .next()
                    .unwrap_or("")
                    .trim_end()
                    .to_string();
                out.push(format!("{} {{ … }}", head));
                i = skip_brace_block(lines, j);
            }
            continue;
        }

        i += 1;
    }
    out
}

fn rust_is_fn(t: &str) -> bool {
    // Catch async fn, pub fn, pub(crate) fn, pub(super) fn, const fn, unsafe fn, default fn.
    let stripped = t
        .trim_start_matches("pub ")
        .trim_start_matches("pub(crate) ")
        .trim_start_matches("pub(super) ")
        .trim_start_matches("pub(self) ")
        .trim_start_matches("default ")
        .trim_start_matches("async ")
        .trim_start_matches("const ")
        .trim_start_matches("unsafe ")
        .trim_start_matches("async ")
        .trim_start_matches("unsafe ");
    stripped.starts_with("fn ")
}

fn rust_is_decl(t: &str) -> bool {
    let stripped = t
        .trim_start_matches("pub ")
        .trim_start_matches("pub(crate) ")
        .trim_start_matches("pub(super) ")
        .trim_start_matches("pub(self) ");
    stripped.starts_with("struct ")
        || stripped.starts_with("enum ")
        || stripped.starts_with("trait ")
        || stripped.starts_with("impl ")
        || stripped.starts_with("impl<")
        || stripped.starts_with("mod ")
}

/// Given that `lines[start_line]` has already opened a `{` block, scan
/// forward and return the index of the line *after* the matching `}`.
fn skip_brace_block(lines: &[&str], start_line: usize) -> usize {
    let mut depth = 0i32;
    let mut seen_open = false;
    let mut i = start_line;
    while i < lines.len() {
        for c in lines[i].chars() {
            if c == '{' {
                depth += 1;
                seen_open = true;
            } else if c == '}' {
                depth -= 1;
            }
        }
        i += 1;
        if seen_open && depth <= 0 {
            return i;
        }
    }
    i
}

// ── TS / JS ─────────────────────────────────────────────────────────────────

fn scan_ts(lines: &[&str], _mode: Mode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();

        if trimmed.starts_with("import ")
            || trimmed.starts_with("export ") && trimmed.contains(" from ")
        {
            out.push(raw.to_string());
            i += 1;
            continue;
        }

        // type / interface — keep entire block (usually small, load-bearing).
        if trimmed.starts_with("type ") || trimmed.starts_with("export type ") {
            // either single-line `type X = ...;` or multi-line — copy until ';'.
            let mut j = i;
            while j < lines.len() {
                out.push(lines[j].to_string());
                if lines[j].trim_end().ends_with(';') || lines[j].trim_end().ends_with('}') {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if trimmed.starts_with("interface ") || trimmed.starts_with("export interface ") {
            // copy whole brace-balanced block.
            let mut depth = 0i32;
            let mut seen = false;
            let mut k = i;
            while k < lines.len() {
                out.push(lines[k].to_string());
                for c in lines[k].chars() {
                    if c == '{' {
                        depth += 1;
                        seen = true;
                    } else if c == '}' {
                        depth -= 1;
                    }
                }
                k += 1;
                if seen && depth <= 0 {
                    break;
                }
            }
            i = k;
            continue;
        }

        // function / class declarations.
        if ts_is_callable(trimmed) {
            // Capture signature up to the line that opens '{'.
            let mut j = i;
            while j < lines.len() && !lines[j].contains('{') {
                j += 1;
            }
            if j == lines.len() {
                out.push(raw.to_string());
                i += 1;
                continue;
            }
            let head_lines = &lines[i..=j];
            let joined = head_lines.join("\n");
            let head = joined
                .split('{')
                .next()
                .unwrap_or("")
                .trim_end()
                .to_string();
            out.push(format!("{} {{ … }}", head));
            i = skip_brace_block(lines, j);
            continue;
        }

        i += 1;
    }
    out
}

fn ts_is_callable(t: &str) -> bool {
    t.starts_with("function ")
        || t.starts_with("export function ")
        || t.starts_with("export default function ")
        || t.starts_with("async function ")
        || t.starts_with("export async function ")
        || t.starts_with("class ")
        || t.starts_with("export class ")
        || t.starts_with("export default class ")
        || t.starts_with("export abstract class ")
}

// ── Fallback ────────────────────────────────────────────────────────────────

fn fallback(lines: &[&str]) -> Vec<String> {
    let n = lines.len();
    if n <= 80 {
        return lines.iter().map(|s| s.to_string()).collect();
    }
    let mut out: Vec<String> = lines[..60].iter().map(|s| s.to_string()).collect();
    out.push(format!("// … {} lines elided …", n - 80));
    out.extend(lines[n - 20..].iter().map(|s| s.to_string()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rust_keeps_use_and_signatures() {
        let src = "use std::io;\n\
                   pub fn foo(x: u32) -> u32 {\n    x + 1\n}\n\
                   fn bar() {\n    println!(\"hi\");\n}\n";
        let r = render(&PathBuf::from("a.rs"), src, Mode::Outline).unwrap();
        assert!(r.contains("use std::io;"));
        assert!(r.contains("pub fn foo"));
        assert!(r.contains("fn bar"));
        assert!(!r.contains("println!"));
    }

    #[test]
    fn python_outline_keeps_imports_and_defs() {
        let src = "import os\nfrom x import y\nCONST = 1\n\
                   def foo(a, b):\n    \"\"\"do stuff\"\"\"\n    return a + b\n\
                   class Bar:\n    def baz(self):\n        return 1\n";
        let r = render(&PathBuf::from("a.py"), src, Mode::Outline).unwrap();
        assert!(r.contains("import os"));
        assert!(r.contains("from x import y"));
        assert!(r.contains("CONST = 1"));
        assert!(r.contains("def foo(a, b):"));
        assert!(r.contains("class Bar:"));
        assert!(r.contains("def baz"));
        assert!(!r.contains("return a + b"));
    }

    #[test]
    fn python_skeleton_drops_docstrings() {
        let src = "def foo():\n    \"\"\"summary\"\"\"\n    return 1\n";
        let r = render(&PathBuf::from("a.py"), src, Mode::Skeleton).unwrap();
        assert!(r.contains("def foo"));
        assert!(!r.contains("summary"));
    }

    #[test]
    fn ts_keeps_imports_and_interfaces() {
        let src = "import { foo } from 'x';\n\
                   export interface Cfg { a: number; b: string; }\n\
                   export function go(x: number) {\n  return x + 1;\n}\n";
        let r = render(&PathBuf::from("a.ts"), src, Mode::Outline).unwrap();
        assert!(r.contains("import { foo }"));
        assert!(r.contains("export interface Cfg"));
        assert!(r.contains("export function go"));
        assert!(!r.contains("return x + 1"));
    }

    #[test]
    fn savings_header_text() {
        let h = savings_header(Mode::Outline, 100, "a\nb\nc");
        assert!(h.contains("100 lines"));
        assert!(h.contains("3 lines"));
        assert!(h.contains("outline mode"));
    }
}
