//! Shared markdown helpers used by the `md_*` tools.
//!
//! - [`parse_frontmatter`] splits a `---\nYAML\n---\n<body>` document into
//!   (frontmatter, body). No frontmatter → returns `(None, original_text)`.
//! - [`outline`] returns a flat list of ATX headings with line numbers,
//!   skipping anything inside fenced code blocks.

use serde_yaml::Value as YamlValue;

/// One ATX heading discovered in a markdown document.
#[derive(Debug, Clone)]
pub struct Heading {
    /// 1..=6 for `#`..`######`.
    pub level: u8,
    /// 1-based line number where the heading appears.
    pub line: u32,
    /// Heading text after the leading `#`s, trimmed.
    pub title: String,
}

/// Split a frontmatter block off the front of `text`.
///
/// Recognised shape:
///
/// ```text
/// ---
/// key: value
/// ---
/// body...
/// ```
///
/// The leading `---` must be the very first line (no BOM, no blank line).
/// Returns `(None, text)` when the document has no frontmatter so the caller
/// can keep the original slice untouched.
pub fn parse_frontmatter(text: &str) -> (Option<YamlValue>, &str) {
    // Strip a UTF-8 BOM if present so the `---` check still matches.
    let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);

    let after_open = match stripped.strip_prefix("---\n") {
        Some(rest) => rest,
        // Tolerate CRLF too.
        None => match stripped.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return (None, text),
        },
    };

    // Find the closing `---` on its own line.
    // Search line-by-line so we don't match `---` inside the YAML body.
    let mut yaml_end_byte: Option<usize> = None;
    let mut body_start_byte: Option<usize> = None;
    let mut cursor = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            yaml_end_byte = Some(cursor);
            body_start_byte = Some(cursor + line.len());
            break;
        }
        cursor += line.len();
    }

    let (Some(yaml_end), Some(body_start)) = (yaml_end_byte, body_start_byte) else {
        return (None, text);
    };

    let yaml_text = &after_open[..yaml_end];
    let body = &after_open[body_start..];

    let parsed = serde_yaml::from_str::<YamlValue>(yaml_text).ok();
    (parsed, body)
}

/// Walk `text` and collect ATX headings, ignoring anything between fenced
/// code blocks (``` or ~~~). Line numbers are 1-based.
pub fn outline(text: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_marker: Option<char> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        let trimmed = raw_line.trim_start();

        // Track fenced code blocks. Toggle when we see the same fence char run.
        if let Some(ch) = trimmed.chars().next() {
            if (ch == '`' || ch == '~') && trimmed.starts_with(&ch.to_string().repeat(3)) {
                if in_fence {
                    if Some(ch) == fence_marker {
                        in_fence = false;
                        fence_marker = None;
                    }
                } else {
                    in_fence = true;
                    fence_marker = Some(ch);
                }
                continue;
            }
        }

        if in_fence {
            continue;
        }

        // ATX heading: 1..=6 leading '#' followed by space.
        if !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if !(1..=6).contains(&level) {
            continue;
        }
        let after = &trimmed[level..];
        // Require a space (or end-of-line) — else `#foo` is not a heading.
        if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
            continue;
        }
        let title = after.trim().trim_end_matches('#').trim().to_string();
        out.push(Heading {
            level: level as u8,
            line: line_no,
            title,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_original() {
        let (fm, body) = parse_frontmatter("hello\nworld\n");
        assert!(fm.is_none());
        assert_eq!(body, "hello\nworld\n");
    }

    #[test]
    fn parses_simple_frontmatter() {
        let src = "---\ntitle: hi\ntags: [a, b]\n---\nbody here\n";
        let (fm, body) = parse_frontmatter(src);
        let fm = fm.expect("frontmatter parsed");
        assert_eq!(fm["title"].as_str(), Some("hi"));
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn outline_collects_atx_only() {
        let src = "# Top\nintro\n## Section\n```\n# not a heading\n```\n### Sub\n";
        let h = outline(src);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].level, 1);
        assert_eq!(h[0].line, 1);
        assert_eq!(h[1].level, 2);
        assert_eq!(h[2].level, 3);
        assert_eq!(h[2].title, "Sub");
    }
}
