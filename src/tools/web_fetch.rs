//! `web_fetch` — pull a URL, run readability, return the main content as text.
//!
//! This is the gateway replacement for Claude's built-in `WebFetch`. Pure local
//! work — no GLM call, no Anthropic tokens. Just `reqwest` + the `readability`
//! Rust port of arc90's algorithm.
//!
//! The output is a tiny markdown doc:
//!
//! ```text
//! # <title>
//!
//! <main content>
//! ```
//!
//! Truncated at `max_chars` (default 8000) so we never hand back a megabyte.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Cursor;
use std::time::Duration;

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const DEFAULT_MAX_CHARS: usize = 8000;
const HARD_MAX_CHARS: usize = 64 * 1024; // 64KB ceiling regardless of caller ask
const FETCH_TIMEOUT_SECS: u64 = 20;
const FETCH_MAX_BYTES: usize = 4 * 1024 * 1024; // 4MB cap on raw HTML

#[derive(Debug, Deserialize)]
struct Args {
    url: String,
    #[serde(default)]
    max_chars: Option<usize>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "web_fetch".into(),
        description: "Fetch a URL and return its main article content as plain text (readability \
             extraction). Use this INSTEAD of Claude's built-in WebFetch — it runs locally \
             with the `readability` Rust crate, costs zero LLM tokens, and strips nav/ads \
             before returning. Output is truncated to `max_chars` (default 8000)."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL." },
                "max_chars": {
                    "type": "integer",
                    "description": "Truncate body to this many chars. Default 8000, hard cap 65536.",
                    "minimum": 200
                }
            },
            "required": ["url"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args { url, max_chars } = serde_json::from_value(args)?;
    let max_chars = max_chars.unwrap_or(DEFAULT_MAX_CHARS).min(HARD_MAX_CHARS);

    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(CallToolResult::error(format!(
            "[web_fetch] only http(s) URLs supported, got: {}",
            url
        )));
    }

    // Parse the URL once up front so readability has a base for relative links.
    let parsed = ::url::Url::parse(&url).with_context(|| format!("invalid URL: {}", url))?;

    // Reqwest GET with our own timeout — `readability::extractor::scrape` would
    // do this for us but binds an internal client we can't tune.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .user_agent("glance-mcp/0.1 (+https://github.com/xtftbwvfp/glance)")
        .build()
        .context("build reqwest client")?;

    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;

    let status = resp.status();
    if !status.is_success() {
        return Ok(CallToolResult::error(format!(
            "[web_fetch] {} returned HTTP {}",
            url, status
        )));
    }

    let bytes = resp.bytes().await.context("read response body")?;
    if bytes.len() > FETCH_MAX_BYTES {
        return Ok(CallToolResult::error(format!(
            "[web_fetch] response too large: {} bytes (>{}MB)",
            bytes.len(),
            FETCH_MAX_BYTES / 1024 / 1024
        )));
    }

    // readability::extractor::extract is sync + CPU-bound. Push it onto blocking.
    let parsed_url = parsed.clone();
    let html_bytes = bytes.to_vec();
    let extracted = tokio::task::spawn_blocking(move || {
        let mut cursor = Cursor::new(html_bytes);
        readability::extractor::extract(&mut cursor, &parsed_url)
            .map_err(|e| anyhow!("readability extract failed: {}", e))
    })
    .await
    .context("readability join")??;

    let title = if extracted.title.trim().is_empty() {
        parsed.host_str().unwrap_or("(no title)").to_string()
    } else {
        extracted.title.trim().to_string()
    };
    let body = extracted.text.trim();

    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&title);
    out.push_str("\n\n");
    out.push_str(body);

    let truncated = out.chars().count() > max_chars;
    if truncated {
        out = out.chars().take(max_chars).collect::<String>();
        out.push_str("\n\n[…truncated by glance.web_fetch]");
    }

    let header = format!("<!-- source: {} -->\n", url);
    Ok(CallToolResult::text(format!("{}{}", header, out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_non_http_url() {
        let r = call(json!({"url": "ftp://example.com/x"})).await.unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn definition_has_required_field() {
        let d = definition();
        assert_eq!(d.name, "web_fetch");
        let req = d.input_schema.get("required").unwrap().as_array().unwrap();
        assert!(req.iter().any(|v| v == "url"));
    }
}
