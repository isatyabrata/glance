//! Vision chat against an OpenAI-compat `/v1/chat/completions` endpoint.
//!
//! Used by the `image_describe` tool. Forces model to GLM-4.5V regardless of
//! the configured chat model — the user's `BackendConfig.model` is for text;
//! vision is a separate sibling SKU on the same base URL.
//!
//! Wire format (OpenAI vision extension):
//! ```json
//! {
//!   "model": "glm-4.5v",
//!   "messages": [
//!     { "role": "user", "content": [
//!         { "type": "text",      "text": "..." },
//!         { "type": "image_url", "image_url": { "url": "data:image/png;base64,..." } }
//!     ]}
//!   ]
//! }
//! ```

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

/// Hardcoded vision model. GLM exposes this on the same base URL as the
/// text models (`https://open.bigmodel.cn/api/paas/v4`). If the user wants a
/// different vision model, swap here — keeping it explicit avoids accidentally
/// sending images to a text-only checkpoint.
const VISION_MODEL: &str = "glm-4.5v";

/// Run a single-turn vision request: one image + one prompt → text answer.
///
/// `image_b64` is raw base64 (no `data:` prefix). `mime` is the image MIME
/// (`image/png`, `image/jpeg`, etc.). Pulls base URL / API key / timeout from
/// the loaded [`crate::config::Config`].
pub async fn vision_chat(image_b64: &str, prompt: &str, mime: &str) -> Result<String> {
    let cfg = crate::config::load_or_default()?;
    let backend = &cfg.backend;
    if backend.api_key.trim().is_empty() {
        return Err(anyhow!("backend api_key is empty (set GLANCE_API_KEY)"));
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(backend.timeout_secs.into()))
        .build()
        .context("build reqwest client")?;

    let url = format!(
        "{}/chat/completions",
        backend.base_url.trim_end_matches('/')
    );
    let data_url = format!("data:{};base64,{}", mime, image_b64);

    let body = json!({
        "model": VISION_MODEL,
        "max_tokens": backend.max_tokens,
        "temperature": 0.2,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }
        ]
    });

    // Same retry policy as openai_compat::Client::chat — see that file for
    // the rationale. Vision endpoints sit behind the same gateway as text
    // endpoints, so they inherit the same 429 / 5xx behavior.
    //
    // Vision has no model fallback (unlike chat) — `glm-4.5v` is the only
    // good vision-capable SKU on the GLM coding plan, and the user's text
    // `fallback_models` list is unlikely to contain vision-capable peers.
    let max_retries = backend.retry.max_retries;
    let base_backoff_ms = backend.retry.base_backoff_ms;
    let max_backoff_secs = backend.retry.max_backoff_secs;

    let mut attempt: u32 = 0;
    let resp = loop {
        let send_result = http
            .post(&url)
            .bearer_auth(&backend.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        let r = match send_result {
            Ok(r) => r,
            Err(e) => {
                let transient = e.is_timeout() || e.is_connect() || e.is_request();
                if transient && attempt < max_retries {
                    let wait = Duration::from_millis(base_backoff_ms * (3u64.pow(attempt)));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = max_retries,
                        wait_ms = wait.as_millis() as u64,
                        err = %e,
                        "vision network error, retrying"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                    continue;
                }
                return Err(anyhow::Error::from(e).context("vision HTTP send"));
            }
        };

        let status = r.status();
        if status.is_success() {
            break r;
        }

        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status.is_server_error() && status != reqwest::StatusCode::NOT_IMPLEMENTED);

        if retryable && attempt < max_retries {
            let retry_after = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|secs| secs.min(max_backoff_secs));
            let wait_ms = retry_after
                .map(|s| s * 1000)
                .unwrap_or(base_backoff_ms * (3u64.pow(attempt)));
            let body_preview = r
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            tracing::warn!(
                attempt = attempt + 1,
                max = max_retries,
                status = status.as_u16(),
                retry_after_secs = retry_after,
                wait_ms,
                body = %body_preview,
                "vision rate-limited / transient error, retrying"
            );
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            attempt += 1;
            continue;
        }

        let raw = r.text().await.unwrap_or_default();
        return Err(anyhow!(
            "vision backend {} returned {}{}: {}",
            url,
            status,
            if attempt > 0 {
                format!(" (after {} retries)", attempt)
            } else {
                String::new()
            },
            raw.chars().take(400).collect::<String>()
        ));
    };

    let parsed: VisionResponse = resp.json().await.context("vision JSON decode")?;
    let text = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content_text())
        .ok_or_else(|| anyhow!("vision backend returned no text"))?;
    Ok(text)
}

#[derive(Debug, Deserialize)]
struct VisionResponse {
    choices: Vec<VisionChoice>,
}

#[derive(Debug, Deserialize)]
struct VisionChoice {
    message: VisionMessage,
}

#[derive(Debug, Deserialize)]
struct VisionMessage {
    /// Some servers return a plain string, others return an array of content
    /// blocks (mostly only on vision *output*, but we handle both to be safe).
    content: Option<Value>,
}

impl VisionMessage {
    fn content_text(&self) -> Option<String> {
        match &self.content {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(blocks)) => {
                let mut buf = String::new();
                for b in blocks {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        buf.push_str(t);
                    }
                }
                if buf.is_empty() {
                    None
                } else {
                    Some(buf)
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_content() {
        let m = VisionMessage {
            content: Some(Value::String("hi".into())),
        };
        assert_eq!(m.content_text().as_deref(), Some("hi"));
    }

    #[test]
    fn parses_block_array_content() {
        let m = VisionMessage {
            content: Some(json!([
                { "type": "text", "text": "alpha " },
                { "type": "text", "text": "beta" }
            ])),
        };
        assert_eq!(m.content_text().as_deref(), Some("alpha beta"));
    }
}
