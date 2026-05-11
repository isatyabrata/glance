//! OpenAI-compat chat completions client with function calling.
//!
//! Compatible with: OpenAI, GLM (Zhipu), DeepSeek, vLLM, Ollama, codex-switcher
//! proxy, and anything else that speaks the standard `/v1/chat/completions`
//! schema.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::config::BackendConfig;

/// One message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String, // "system" | "user" | "assistant" | "tool"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Set on `role: "tool"` messages — references the assistant's tool_call id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Set on `role: "tool"` messages — name of the tool that produced this output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// DeepSeek thinking-mode reasoning payload. The API requires this field
    /// to be echoed back in subsequent requests when the model returns it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }
    pub fn tool(
        call_id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
            reasoning_content: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Arguments arrive as a JSON-encoded string per OpenAI spec.
    pub arguments: String,
}

/// Function tool definition (what we send to the model).
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: &'static str, // "function"
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    // Cache-friendly order: tools → system (in messages[0]) → conversation.
    // Anthropic-compat backends use field position to compute the cacheable
    // prefix; OpenAI / DeepSeek don't care about order but hash by content,
    // so putting the most stable fields first never hurts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpec>,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// Token accounting normalised across all OpenAI-compatible backends.
///
/// `cached_tokens` is read from whichever of these shapes the backend uses:
///   - OpenAI o1 / DeepSeek: `prompt_tokens_details.cached_tokens`
///   - Anthropic: `cache_read_input_tokens`
///   - DeepSeek classic: `prompt_cache_hit_tokens`
///
/// `cache_creation_tokens` is Anthropic-only (cache-WRITE billed at 1.25×).
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_tokens: u32,
    pub cache_creation_tokens: u32,
}

impl<'de> serde::Deserialize<'de> for Usage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        let pull = |k: &str| -> u32 {
            raw.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as u32
        };
        let prompt_tokens = pull("prompt_tokens");
        let completion_tokens = pull("completion_tokens");
        let total_tokens = pull("total_tokens");
        // Cache read, multi-source.
        let cached_tokens = raw
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .or_else(|| raw.get("cache_read_input_tokens").and_then(|v| v.as_u64()).map(|n| n as u32))
            .or_else(|| raw.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()).map(|n| n as u32))
            .unwrap_or(0);
        // Cache write (Anthropic only).
        let cache_creation_tokens = raw
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(0);
        Ok(Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            cache_creation_tokens,
        })
    }
}

pub struct Client {
    http: reqwest::Client,
    cfg: BackendConfig,
}

impl Client {
    pub fn new(cfg: BackendConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.into()))
            .build()
            .context("build reqwest client")?;
        Ok(Self { http, cfg })
    }

    /// One round trip. Returns the assistant's message + token usage.
    /// The caller's loop decides what to do with `tool_calls` (run them, append
    /// `tool` messages, call `chat` again).
    ///
    /// Transient backend failures (429 / 502 / 503 / 504, network errors) are
    /// retried per `BackendConfig.retry` (default 3 attempts with exponential
    /// backoff ≈1s / 3s / 9s, honoring `Retry-After` capped at 30s). When all
    /// retries on the primary `model` are exhausted, the client falls
    /// through `BackendConfig.fallback_models` in order — each fallback gets
    /// its own full retry budget. Non-transient errors (4xx other than 429,
    /// auth, bad request) bubble up immediately without trying fallbacks.
    pub async fn chat(&self, messages: &[Message], tools: Vec<ToolSpec>) -> Result<ChatTurn> {
        let mut models: Vec<String> = vec![self.cfg.model.clone()];
        models.extend(self.cfg.fallback_models.iter().cloned());

        let mut last_transient_err: Option<anyhow::Error> = None;
        for (idx, model) in models.iter().enumerate() {
            match self.chat_one_model(model, messages, tools.clone()).await {
                Ok(turn) => {
                    if idx > 0 {
                        tracing::info!(
                            model,
                            fell_back_from = self.cfg.model.as_str(),
                            "chat succeeded on fallback model"
                        );
                    }
                    return Ok(turn);
                }
                Err(ChatAttemptError::Hard(e)) => return Err(e),
                Err(ChatAttemptError::Transient(e)) => {
                    let next_model = models.get(idx + 1).map(String::as_str);
                    if let Some(next) = next_model {
                        tracing::warn!(
                            model,
                            next_model = next,
                            err = %e,
                            "model exhausted retries, falling through to next"
                        );
                    } else {
                        tracing::warn!(
                            model,
                            err = %e,
                            "all models exhausted, giving up"
                        );
                    }
                    last_transient_err = Some(e);
                }
            }
        }
        Err(last_transient_err.unwrap_or_else(|| anyhow!("backend has no models configured")))
    }

    /// Single-model attempt with the configured retry budget. Errors are
    /// classified so the outer loop knows whether to fall through to the
    /// next model (transient) or give up (hard).
    async fn chat_one_model(
        &self,
        model: &str,
        messages: &[Message],
        tools: Vec<ToolSpec>,
    ) -> std::result::Result<ChatTurn, ChatAttemptError> {
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let req = ChatRequest {
            model,
            messages,
            tools,
            max_tokens: Some(self.cfg.max_tokens),
            temperature: Some(0.2),
        };

        let max_retries = self.cfg.retry.max_retries;
        let base_backoff_ms = self.cfg.retry.base_backoff_ms;
        let max_backoff_secs = self.cfg.retry.max_backoff_secs;

        let mut attempt: u32 = 0;
        loop {
            let send_result = self
                .http
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .header("content-type", "application/json")
                .json(&req)
                .send()
                .await;

            let resp = match send_result {
                Ok(r) => r,
                Err(e) => {
                    let transient = e.is_timeout() || e.is_connect() || e.is_request();
                    if transient && attempt < max_retries {
                        let wait = Duration::from_millis(base_backoff_ms * (3u64.pow(attempt)));
                        tracing::warn!(
                            model,
                            attempt = attempt + 1,
                            max = max_retries,
                            wait_ms = wait.as_millis() as u64,
                            err = %e,
                            "backend network error, retrying"
                        );
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(ChatAttemptError::Transient(
                        anyhow::Error::from(e).context("backend HTTP send"),
                    ));
                }
            };

            let status = resp.status();
            if status.is_success() {
                let parsed: ChatResponse = resp
                    .json()
                    .await
                    .context("backend JSON decode")
                    .map_err(ChatAttemptError::Hard)?;
                let choice = parsed.choices.into_iter().next().ok_or_else(|| {
                    ChatAttemptError::Hard(anyhow!("backend returned no choices"))
                })?;
                return Ok(ChatTurn {
                    message: choice.message,
                    finish_reason: choice.finish_reason,
                    usage: parsed.usage,
                });
            }

            // Retryable status codes: 429 + 5xx (excluding 501 Not Implemented).
            let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (status.is_server_error() && status != reqwest::StatusCode::NOT_IMPLEMENTED);

            if retryable && attempt < max_retries {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|secs| secs.min(max_backoff_secs));
                let wait_ms = retry_after
                    .map(|s| s * 1000)
                    .unwrap_or(base_backoff_ms * (3u64.pow(attempt)));
                let body_preview = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>();
                tracing::warn!(
                    model,
                    attempt = attempt + 1,
                    max = max_retries,
                    status = status.as_u16(),
                    retry_after_secs = retry_after,
                    wait_ms,
                    body = %body_preview,
                    "backend rate-limited / transient error, retrying"
                );
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                attempt += 1;
                continue;
            }

            // Classify the final failure for the outer model-fallback loop.
            let body = resp.text().await.unwrap_or_default();
            let body_preview = body.chars().take(300).collect::<String>();
            let err = anyhow!(
                "backend {} returned {}{}: {}",
                url,
                status,
                if attempt > 0 {
                    format!(" (after {} retries)", attempt)
                } else {
                    String::new()
                },
                body_preview
            );
            // 429 + 5xx that ran out of retries → transient (try fallback).
            // Everything else (auth, bad request, 404 etc) → hard, bubble up.
            return Err(if retryable {
                ChatAttemptError::Transient(err)
            } else {
                ChatAttemptError::Hard(err)
            });
        }
    }
}

/// Internal: classify a single-model attempt outcome so the outer
/// fallback loop knows what to do.
enum ChatAttemptError {
    /// Auth, bad request, 404, schema decode — fallback won't help.
    Hard(anyhow::Error),
    /// 429 / 5xx / network — try the next model in `fallback_models`.
    Transient(anyhow::Error),
}

#[derive(Debug)]
pub struct ChatTurn {
    pub message: Message,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}
