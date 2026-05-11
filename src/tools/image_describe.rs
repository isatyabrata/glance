//! `image_describe` — hand a local image to GLM-4.5V and return its prose
//! description.
//!
//! The point: image tokens are *expensive* on Anthropic's side. Routing image
//! analysis to GLM (or any GLM-vision-compat backend) keeps the visual reading
//! off Claude's context window — the caller just gets cheap text back.
//!
//! Reads the file → base64 → calls [`crate::backend::vision_chat`].

use anyhow::Result;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024; // 10MB — most APIs cap around here

#[derive(Debug, Deserialize)]
struct Args {
    image_path: String,
    #[serde(default)]
    question: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "image_describe".into(),
        description:
            "Describe a local image via GLM-4.5V (or whatever the backend exposes as the vision \
             SKU). Use this INSTEAD of letting Claude see the image natively — image tokens are \
             expensive, prose tokens are cheap. Supported formats: png, jpg/jpeg, webp, gif. \
             Returns a single text block with the model's answer."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "image_path": {
                    "type": "string",
                    "description": "Absolute or cwd-relative path to a png/jpg/jpeg/webp/gif file."
                },
                "question": {
                    "type": "string",
                    "description": "What to ask about the image. Default: 'Describe this image in detail.'"
                }
            },
            "required": ["image_path"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        image_path,
        question,
    } = serde_json::from_value(args)?;
    let question = question.unwrap_or_else(|| "Describe this image in detail.".to_string());

    let path = resolve_path(&image_path);
    let mime = match mime_for(&path) {
        Some(m) => m,
        None => {
            return Ok(CallToolResult::error(format!(
                "[image_describe] unsupported extension for {} (need png/jpg/jpeg/webp/gif)",
                path.display()
            )));
        }
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => {
            return Ok(CallToolResult::error(format!(
                "[image_describe] cannot stat {}: {}",
                path.display(),
                e
            )));
        }
    };
    if meta.len() > MAX_IMAGE_BYTES {
        return Ok(CallToolResult::error(format!(
            "[image_describe] {} too large: {} bytes (>{}MB)",
            path.display(),
            meta.len(),
            MAX_IMAGE_BYTES / 1024 / 1024
        )));
    }

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(CallToolResult::error(format!(
                "[image_describe] read {} failed: {}",
                path.display(),
                e
            )));
        }
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    match crate::backend::vision_chat(&b64, &question, mime).await {
        Ok(text) => Ok(CallToolResult::text(text)),
        Err(e) => Ok(CallToolResult::error(format!(
            "[image_describe] vision backend failed: {}",
            e
        ))),
    }
}

fn resolve_path(s: &str) -> PathBuf {
    let p = Path::new(s);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}

fn mime_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_for_known_extensions() {
        assert_eq!(mime_for(Path::new("x.PNG")), Some("image/png"));
        assert_eq!(mime_for(Path::new("x.jpg")), Some("image/jpeg"));
        assert_eq!(mime_for(Path::new("x.jpeg")), Some("image/jpeg"));
        assert_eq!(mime_for(Path::new("x.webp")), Some("image/webp"));
        assert_eq!(mime_for(Path::new("x.gif")), Some("image/gif"));
        assert_eq!(mime_for(Path::new("x.tiff")), None);
        assert_eq!(mime_for(Path::new("noext")), None);
    }

    #[tokio::test]
    async fn rejects_unsupported_extension() {
        let r = call(json!({"image_path": "/tmp/foo.tiff"})).await.unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
