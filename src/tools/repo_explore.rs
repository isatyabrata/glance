//! `repo_explore` — talk to GitHub or zread.ai so the caller doesn't have to
//! clone-and-grep.
//!
//! Three actions:
//! - `structure`: list repo file tree.
//! - `search_doc`: code/doc search.
//! - `read_file`: fetch a single file by path.
//!
//! Two backends:
//! - `github` (default for structure/read_file): direct GitHub REST API via
//!   octocrab. Optional `GITHUB_TOKEN` env var. Anonymous = 60 req/h limit;
//!   `search_doc` requires a token (GitHub blocks anonymous code search).
//! - `zread`: routes through GLM's `zread` MCP (`https://open.bigmodel.cn/api
//!   /mcp/zread/mcp`). Counts against the user's 1000/4000 monthly MCP quota.
//!   Works without GITHUB_TOKEN; only covers repos zread.ai has indexed.
//!
//! Default behavior is `auto`: GitHub first, zread fallback when GitHub
//! returns rate-limit / auth-required. Caller can pin `backend: "github"` or
//! `backend: "zread"` to skip the fallback.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use octocrab::Octocrab;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backend::glm_mcp;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};

const ZREAD_ENDPOINT: &str = "https://open.bigmodel.cn/api/mcp/zread/mcp";

const READ_FILE_CAP: usize = 16 * 1024;
const STRUCTURE_DEPTH: u32 = 2; // root + 2 nested levels feels enough
const STRUCTURE_MAX_ENTRIES: usize = 200;
const SEARCH_TOP_K: u8 = 10;

#[derive(Debug, Deserialize)]
struct Args {
    /// `owner/name`
    repo: String,
    /// `structure` | `search_doc` | `read_file`
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    query: Option<String>,
    /// `auto` (default) | `github` | `zread`. `auto` tries GitHub first and
    /// falls back to zread on rate limit / auth refusal.
    #[serde(default)]
    backend: Option<String>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "repo_explore".into(),
        description: "Explore a GitHub repo without cloning. Actions: \
             `structure` (file tree), `search_doc` (code/doc search), \
             `read_file` (fetch one file, truncated to 16KB). \
             Backends: `auto` (default — GitHub first, zread fallback on rate limit), \
             `github` (REST API, needs GITHUB_TOKEN for code search), \
             `zread` (GLM's zread MCP — covers indexed public repos, counts against GLM MCP quota). \
             Use this INSTEAD of `git clone` + grep."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "repo":    { "type": "string", "description": "GitHub repo as `owner/name`." },
                "action":  { "type": "string", "enum": ["structure", "search_doc", "read_file"] },
                "path":    { "type": "string", "description": "File path inside the repo (read_file)." },
                "query":   { "type": "string", "description": "Search query (search_doc)." },
                "backend": { "type": "string", "enum": ["auto", "github", "zread"], "description": "Default `auto`." }
            },
            "required": ["repo", "action"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let Args {
        repo,
        action,
        path,
        query,
        backend,
    } = serde_json::from_value(args)?;

    let (owner, name) = parse_repo(&repo)
        .ok_or_else(|| anyhow!("invalid repo `{}` — expected owner/name", repo))?;
    let backend_choice = backend.as_deref().unwrap_or("auto");
    let has_github_token = resolve_github_token().is_some();

    match action.as_str() {
        "structure" => match backend_choice {
            "zread" => zread_structure(&repo, path.as_deref()).await,
            _ => {
                let octo = build_client()?;
                let res = action_structure(&octo, &owner, &name).await;
                if backend_choice == "auto" && should_fallback(&res) {
                    tracing::info!(repo = %repo, "structure: github failed, falling back to zread");
                    zread_structure(&repo, path.as_deref()).await
                } else {
                    res
                }
            }
        },
        "search_doc" => {
            let q = query.ok_or_else(|| anyhow!("`query` is required for action=search_doc"))?;
            match backend_choice {
                "zread" => zread_search(&repo, &q).await,
                "auto" if !has_github_token => {
                    // Anonymous code search is rejected by GitHub — go zread.
                    tracing::info!(repo = %repo, "search_doc: no GITHUB_TOKEN, using zread");
                    zread_search(&repo, &q).await
                }
                _ => {
                    let octo = build_client()?;
                    let res = action_search(&octo, &owner, &name, &q).await;
                    if backend_choice == "auto" && should_fallback(&res) {
                        tracing::info!(repo = %repo, "search_doc: github failed, falling back to zread");
                        zread_search(&repo, &q).await
                    } else {
                        res
                    }
                }
            }
        }
        "read_file" => {
            let p = path.ok_or_else(|| anyhow!("`path` is required for action=read_file"))?;
            match backend_choice {
                "zread" => zread_read_file(&repo, &p).await,
                _ => {
                    let octo = build_client()?;
                    let res = action_read_file(&octo, &owner, &name, &p).await;
                    if backend_choice == "auto" && should_fallback(&res) {
                        tracing::info!(repo = %repo, path = %p, "read_file: github failed, falling back to zread");
                        zread_read_file(&repo, &p).await
                    } else {
                        res
                    }
                }
            }
        }
        other => Ok(CallToolResult::error(format!(
            "[repo_explore] unknown action `{}` (try structure | search_doc | read_file)",
            other
        ))),
    }
}

/// Inspect a CallToolResult to decide whether the GitHub backend hit a
/// transient rate / auth issue and zread should be tried instead.
fn should_fallback(res: &Result<CallToolResult>) -> bool {
    match res {
        Err(_) => true,
        Ok(r) => {
            if !r.is_error.unwrap_or(false) {
                return false;
            }
            let body: String = r
                .content
                .iter()
                .map(|b| match b {
                    crate::mcp::protocol::ToolContentBlock::Text { text } => text.as_str(),
                })
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            body.contains("403")
                || body.contains("429")
                || body.contains("rate limit")
                || body.contains("github_token")
                || body.contains("401")
                || body.contains("authentication")
        }
    }
}

// ── zread (GLM MCP) backend ────────────────────────────────────────────────

async fn zread_call(tool: &str, args: Value) -> Result<CallToolResult> {
    let cfg = crate::config::load_or_default()?;
    let key = cfg.backend.api_key.trim();
    if key.is_empty() {
        return Ok(CallToolResult::error(
            "[repo_explore/zread] backend.api_key is empty — set it in ~/.glance/config.toml"
                .to_string(),
        ));
    }
    match glm_mcp::call_tool(key, ZREAD_ENDPOINT, tool, args).await {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(CallToolResult::text(format!(
                    "(zread {} returned empty)",
                    tool
                )))
            } else {
                Ok(CallToolResult::text(text))
            }
        }
        Err(e) => Ok(CallToolResult::error(format!(
            "[repo_explore/zread] {}: {}",
            tool, e
        ))),
    }
}

async fn zread_structure(repo: &str, dir_path: Option<&str>) -> Result<CallToolResult> {
    let mut args = json!({ "repo_name": repo });
    if let Some(p) = dir_path {
        args["dir_path"] = json!(p);
    }
    zread_call("get_repo_structure", args).await
}

async fn zread_search(repo: &str, query: &str) -> Result<CallToolResult> {
    zread_call(
        "search_doc",
        json!({ "repo_name": repo, "query": query, "language": "en" }),
    )
    .await
}

async fn zread_read_file(repo: &str, file_path: &str) -> Result<CallToolResult> {
    zread_call(
        "read_file",
        json!({ "repo_name": repo, "file_path": file_path }),
    )
    .await
}

fn parse_repo(s: &str) -> Option<(String, String)> {
    let mut it = s.splitn(2, '/');
    let o = it.next()?.trim();
    let n = it.next()?.trim();
    if o.is_empty() || n.is_empty() {
        return None;
    }
    Some((o.to_string(), n.to_string()))
}

fn build_client() -> Result<Octocrab> {
    let mut b = Octocrab::builder();
    if let Some(tok) = resolve_github_token() {
        b = b.personal_token(tok);
    }
    b.build().context("build octocrab client")
}

/// Look up GitHub token: env var first (CI / Docker / per-shell wins),
/// then `~/.glance/config.toml` `[tokens]` section. Returns `None` if
/// neither is set, in which case anonymous GitHub calls are used.
fn resolve_github_token() -> Option<String> {
    crate::config::load_or_default()
        .ok()
        .and_then(|c| c.tokens.resolved_github())
}

async fn action_structure(octo: &Octocrab, owner: &str, name: &str) -> Result<CallToolResult> {
    let mut out = String::new();
    let mut count = 0usize;
    walk_tree(octo, owner, name, "", 0, &mut out, &mut count).await?;
    if out.is_empty() {
        return Ok(CallToolResult::text(format!(
            "(repo {}/{} has no listable contents at root)",
            owner, name
        )));
    }
    Ok(CallToolResult::text(out))
}

async fn walk_tree(
    octo: &Octocrab,
    owner: &str,
    name: &str,
    path: &str,
    depth: u32,
    out: &mut String,
    count: &mut usize,
) -> Result<()> {
    if depth > STRUCTURE_DEPTH || *count >= STRUCTURE_MAX_ENTRIES {
        return Ok(());
    }
    let handler = octo.repos(owner, name);
    let mut req = handler.get_content();
    if !path.is_empty() {
        req = req.path(path);
    }
    let items = match req.send().await {
        Ok(c) => c.items,
        Err(e) => {
            // Don't fail the whole walk if a subdir 404s — just skip it.
            tracing::warn!("repo_explore walk skip {}/{}/{}: {}", owner, name, path, e);
            return Ok(());
        }
    };

    for entry in items {
        if *count >= STRUCTURE_MAX_ENTRIES {
            out.push_str("…(structure truncated)\n");
            return Ok(());
        }
        let indent = "  ".repeat(depth as usize);
        let suffix = if entry.r#type == "dir" { "/" } else { "" };
        out.push_str(&format!("{}{}{}\n", indent, entry.path, suffix));
        *count += 1;
        if entry.r#type == "dir" && depth < STRUCTURE_DEPTH {
            Box::pin(walk_tree(
                octo,
                owner,
                name,
                &entry.path,
                depth + 1,
                out,
                count,
            ))
            .await?;
        }
    }
    Ok(())
}

async fn action_search(
    octo: &Octocrab,
    owner: &str,
    name: &str,
    query: &str,
) -> Result<CallToolResult> {
    let q = format!("{} repo:{}/{}", query, owner, name);
    let page = match octo.search().code(&q).per_page(SEARCH_TOP_K).send().await {
        Ok(p) => p,
        Err(e) => {
            return Ok(CallToolResult::error(format!(
                "[repo_explore] code search failed: {} (note: GitHub code search needs a GITHUB_TOKEN)",
                e
            )));
        }
    };

    if page.items.is_empty() {
        return Ok(CallToolResult::text(format!(
            "(no code-search hits for `{}` in {}/{})",
            query, owner, name
        )));
    }

    let mut out = format!(
        "# code search hits for `{}` in {}/{}\n\n",
        query, owner, name
    );
    for hit in page.items.iter().take(SEARCH_TOP_K as usize) {
        out.push_str(&format!("- `{}` — {}\n", hit.path, hit.html_url));
    }
    Ok(CallToolResult::text(out))
}

async fn action_read_file(
    octo: &Octocrab,
    owner: &str,
    name: &str,
    path: &str,
) -> Result<CallToolResult> {
    let handler = octo.repos(owner, name);
    let items = match handler.get_content().path(path).send().await {
        Ok(c) => c.items,
        Err(e) => {
            return Ok(CallToolResult::error(format!(
                "[repo_explore] read_file {}/{} {} failed: {}",
                owner, name, path, e
            )));
        }
    };
    let entry = items
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no content for {}/{} {}", owner, name, path))?;

    if entry.r#type != "file" {
        return Ok(CallToolResult::error(format!(
            "[repo_explore] {} is a {}, not a file",
            path, entry.r#type
        )));
    }

    // Prefer the helper, fall back to manual base64 decode.
    let decoded = entry.decoded_content().or_else(|| {
        let raw = entry.content.as_deref().unwrap_or("").replace('\n', "");
        base64::engine::general_purpose::STANDARD
            .decode(raw.as_bytes())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    });
    let mut text = decoded.unwrap_or_else(|| "(no decodable content)".to_string());
    let truncated = text.len() > READ_FILE_CAP;
    if truncated {
        text.truncate(READ_FILE_CAP);
        text.push_str("\n\n[…truncated by glance.repo_explore]");
    }

    let header = format!("<!-- {}/{}:{} -->\n", owner, name, path);
    Ok(CallToolResult::text(format!("{}{}", header, text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_string() {
        assert_eq!(
            parse_repo("rust-lang/rust"),
            Some(("rust-lang".into(), "rust".into()))
        );
        assert_eq!(parse_repo(""), None);
        assert_eq!(parse_repo("noslash"), None);
        assert_eq!(parse_repo("/empty"), None);
    }

    #[tokio::test]
    async fn rejects_bad_repo() {
        let r = call(json!({ "repo": "noslash", "action": "structure" })).await;
        assert!(r.is_err());
    }

    #[test]
    fn fallback_triggers_on_github_403() {
        let res: Result<CallToolResult> = Ok(CallToolResult::error(
            "[repo_explore] code search failed: HTTP 403 Forbidden — rate limit".to_string(),
        ));
        assert!(should_fallback(&res));
    }

    #[test]
    fn fallback_skips_clean_result() {
        let res: Result<CallToolResult> = Ok(CallToolResult::text("file tree".to_string()));
        assert!(!should_fallback(&res));
    }

    #[test]
    fn fallback_triggers_on_top_level_err() {
        let res: Result<CallToolResult> = Err(anyhow!("octocrab: connect failed"));
        assert!(should_fallback(&res));
    }
}
