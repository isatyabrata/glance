//! Obsidian vault helpers: path resolution, wikilink/tag parsing, minimal
//! frontmatter splitter. Used by `obsidian_*` tools.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// A `[[Note]]` or `[[Note|alias]]` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    pub target: String,
    pub alias: Option<String>,
}

/// Resolve the active vault path.
///
/// Priority:
/// 1. `cfg.obsidian.vault` if non-empty.
/// 2. A line `mcp.obsidian_vault: <path>` or `obsidian_vault: <path>` inside
///    `./AGENTS.md` or `./CLAUDE.md` in the current working directory.
/// 3. Hardcoded iCloud default
///    `~/Library/Mobile Documents/com~apple~CloudDocs/ObsidianVault`.
///
/// Errors if the resolved path doesn't exist on disk.
pub fn resolve_vault(cfg: &Config) -> Result<PathBuf> {
    let candidate = if !cfg.obsidian.vault.trim().is_empty() {
        PathBuf::from(expand_tilde(cfg.obsidian.vault.trim()))
    } else if let Some(p) = read_vault_from_project_docs()? {
        p
    } else {
        default_icloud_vault()?
    };

    if !candidate.exists() {
        return Err(anyhow!(
            "obsidian vault not found at {}",
            candidate.display()
        ));
    }
    Ok(candidate)
}

fn default_icloud_vault() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home
        .join("Library")
        .join("Mobile Documents")
        .join("com~apple~CloudDocs")
        .join("ObsidianVault"))
}

fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    s.to_string()
}

fn read_vault_from_project_docs() -> Result<Option<PathBuf>> {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let p = cwd.join(name);
        if !p.exists() {
            continue;
        }
        let raw = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in raw.lines() {
            let trimmed = line.trim_start_matches(|c: char| {
                c == '-' || c == '*' || c == '>' || c.is_whitespace()
            });
            for key in ["mcp.obsidian_vault:", "obsidian_vault:"] {
                if let Some(rest) = trimmed.strip_prefix(key) {
                    let val = rest.trim().trim_matches('"').trim_matches('\'');
                    if !val.is_empty() {
                        return Ok(Some(PathBuf::from(expand_tilde(val))));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Extract `[[Note Name]]` and `[[Note Name|alias]]` references from `text`.
/// Skips bare `[[]]` or anything containing a newline inside the brackets.
pub fn extract_wikilinks(text: &str) -> Vec<WikiLink> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find closing ]]
            let start = i + 2;
            let mut j = start;
            let mut closed = false;
            while j + 1 < bytes.len() {
                if bytes[j] == b'\n' {
                    break;
                }
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if closed {
                let inner = &text[start..j];
                if !inner.is_empty() {
                    let (target, alias) = if let Some((t, a)) = inner.split_once('|') {
                        (t.trim().to_string(), Some(a.trim().to_string()))
                    } else {
                        (inner.trim().to_string(), None)
                    };
                    if !target.is_empty() {
                        out.push(WikiLink { target, alias });
                    }
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Extract `#tag` style tags (allowing dashes, underscores, slashes for nested
/// tags). Skips fenced code blocks (```...```) and inline `# heading` markdown
/// (a `#` followed by a space at the start of a line).
pub fn extract_tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        // Skip ATX-style headings: `# heading`, `## ...`. Only when there's a
        // space directly after the `#`.
        let mut hash_count = 0;
        for c in trimmed.chars() {
            if c == '#' {
                hash_count += 1;
            } else {
                if hash_count > 0 && c == ' ' {
                    // Heading line; still scan for tags AFTER the heading text
                    // by jumping to character-by-character scanning below but
                    // treat the leading hashes as not-tags.
                }
                break;
            }
        }

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '`' {
                // Skip inline code spans — find next backtick.
                if let Some(rel) = line[i + 1..].find('`') {
                    i += rel + 2;
                    continue;
                } else {
                    break;
                }
            }
            if c == '#' {
                // Boundary: # must be at start of line or preceded by whitespace
                // or punctuation that isn't itself a word char.
                let prev_ok = if i == 0 {
                    true
                } else {
                    let p = bytes[i - 1] as char;
                    !(p.is_alphanumeric() || p == '_' || p == '-' || p == '/')
                };
                if !prev_ok {
                    i += 1;
                    continue;
                }

                // If next char is space → markdown heading marker, skip.
                let next = bytes.get(i + 1).map(|b| *b as char);
                match next {
                    Some(ch) if ch.is_alphanumeric() || ch == '_' || ch == '-' => {
                        // Read tag chars
                        let start = i + 1;
                        let mut j = start;
                        while j < bytes.len() {
                            let cc = bytes[j] as char;
                            if cc.is_alphanumeric() || cc == '_' || cc == '-' || cc == '/' {
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        if j > start {
                            let tag = &line[start..j];
                            // Reject pure-numeric (e.g. `#1`) — unlikely a tag.
                            if !tag.chars().all(|c| c.is_ascii_digit()) {
                                out.push(tag.to_string());
                            }
                        }
                        i = j;
                        continue;
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    out
}

/// Convert a note name (`Note Name` or `Folder/Note Name`) to a path inside
/// the vault. If the name already ends with `.md` it's left alone.
pub fn note_path_for_name(vault: &Path, name: &str) -> PathBuf {
    let name = name.trim_start_matches('/').trim();
    if name.ends_with(".md") {
        vault.join(name)
    } else {
        vault.join(format!("{}.md", name))
    }
}

/// Split `---\nYAML\n---\nbody` into `(Some(yaml), body)`. If there's no
/// frontmatter block, returns `(None, original_text)`.
pub fn parse_frontmatter(text: &str) -> (Option<String>, String) {
    if !text.starts_with("---") {
        return (None, text.to_string());
    }
    let after_first = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(s) => s,
        None => return (None, text.to_string()),
    };
    // Find the closing `---` on its own line.
    let mut idx = 0usize;
    for line in after_first.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let yaml = &after_first[..idx];
            let rest_start = idx + line.len();
            let body = &after_first[rest_start..];
            return (Some(yaml.to_string()), body.to_string());
        }
        idx += line.len();
    }
    (None, text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikilinks_basic() {
        let links = extract_wikilinks("see [[Note A]] and [[Note B|alias]] also [[]]");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "Note A");
        assert_eq!(links[0].alias, None);
        assert_eq!(links[1].target, "Note B");
        assert_eq!(links[1].alias.as_deref(), Some("alias"));
    }

    #[test]
    fn tags_skip_headings_and_code() {
        let text = "# heading not a tag\n#real-tag here\n```\n#code-tag\n```\n#nested/path\n";
        let tags = extract_tags(text);
        assert!(tags.contains(&"real-tag".to_string()));
        assert!(tags.contains(&"nested/path".to_string()));
        assert!(!tags.contains(&"code-tag".to_string()));
    }

    #[test]
    fn note_path_appends_md() {
        let p = note_path_for_name(Path::new("/v"), "Foo Bar");
        assert_eq!(p, PathBuf::from("/v/Foo Bar.md"));
        let p2 = note_path_for_name(Path::new("/v"), "x.md");
        assert_eq!(p2, PathBuf::from("/v/x.md"));
        let p3 = note_path_for_name(Path::new("/v"), "Folder/Note");
        assert_eq!(p3, PathBuf::from("/v/Folder/Note.md"));
    }

    #[test]
    fn frontmatter_split() {
        let (fm, body) = parse_frontmatter("---\ntitle: hi\n---\nhello\n");
        assert_eq!(fm.as_deref(), Some("title: hi\n"));
        assert_eq!(body, "hello\n");

        let (fm, body) = parse_frontmatter("no frontmatter here");
        assert!(fm.is_none());
        assert_eq!(body, "no frontmatter here");
    }
}
