//! Tauri commands surfaced to the webview.
//!
//! Reads/writes `~/.glance/config.toml` via the `glance` library, tails
//! `~/.glance/events.jsonl`, computes today's stats, and pings the backend.

use anyhow::Context;
use chrono::{DateTime, Utc};
use glance::config::{user_config_path, Config, UpstreamMcp};
use glance::events::ToolEvent;
use glance::mcp_aggregator::{self, SmokeTestResult, UpstreamStatusSnapshot, UpstreamTemplate};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

// ── Config get/save ─────────────────────────────────────────────────────────

#[tauri::command]
pub fn get_config() -> Result<Config, String> {
    glance::config::load_or_default().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_config(cfg: Config) -> Result<(), String> {
    let path = user_config_path().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let toml = toml::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, toml).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn get_config_path() -> Result<String, String> {
    user_config_path()
        .map(|p| p.display().to_string())
        .map_err(|e| e.to_string())
}

// ── Tool list (dynamic, from ToolsConfig::default) ──────────────────────────

#[derive(Serialize)]
pub struct ToolEntry {
    pub key: String,
    pub default_on: bool,
    pub category: &'static str,
}

#[tauri::command]
pub fn list_tool_toggles() -> Result<Vec<ToolEntry>, String> {
    let defaults = glance::config::ToolsConfig::default();
    let v = serde_json::to_value(&defaults).map_err(|e| e.to_string())?;
    let obj = v
        .as_object()
        .ok_or_else(|| "ToolsConfig is not an object".to_string())?;
    let mut out = Vec::with_capacity(obj.len());
    for (k, val) in obj {
        let default_on = val.as_bool().unwrap_or(false);
        let category = if default_on { "read" } else { "write" };
        out.push(ToolEntry {
            key: k.clone(),
            default_on,
            category,
        });
    }
    out.sort_by(|a, b| {
        b.default_on
            .cmp(&a.default_on)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(out)
}

// ── Chrome bridge (extension + native host) ─────────────────────────────────

#[tauri::command]
pub fn chrome_status() -> Result<glance::install::chrome::ChromeStatus, String> {
    glance::install::chrome::status().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn chrome_install() -> Result<glance::install::chrome::InstallReport, String> {
    glance::install::chrome::install().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn chrome_uninstall() -> Result<(), String> {
    glance::install::chrome::uninstall().map_err(|e| e.to_string())
}

/// Open chrome://extensions in the user's Chrome.
#[tauri::command]
pub fn chrome_open_extensions_page() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-a")
        .arg("Google Chrome")
        .arg("chrome://extensions")
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reveal `~/.glance/chrome-bridge/extension/` in Finder so the user can
/// drag-drop into Chrome's "Load unpacked" picker.
#[tauri::command]
pub fn chrome_open_extension_dir() -> Result<(), String> {
    let p = glance::install::chrome::ext_dir().map_err(|e| e.to_string())?;
    std::process::Command::new("open")
        .arg(p)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── Chrome adapters (YAML site recipes) ─────────────────────────────────────

#[derive(Serialize)]
pub struct AdapterSummary {
    pub name: String,
    pub description: Option<String>,
    pub match_url: Option<String>,
    pub args: Vec<String>,
    pub source_path: Option<String>,
}

#[tauri::command]
pub fn chrome_adapter_list() -> Result<Vec<AdapterSummary>, String> {
    let m = glance::install::chrome_adapters::load_all().map_err(|e| e.to_string())?;
    Ok(m.into_values()
        .map(|a| AdapterSummary {
            name: a.name,
            description: a.description,
            match_url: a.match_url,
            args: a.args.into_iter().map(|x| x.name).collect(),
            source_path: a.source_path.map(|p| p.display().to_string()),
        })
        .collect())
}

#[tauri::command]
pub fn chrome_adapter_get(name: String) -> Result<String, String> {
    glance::install::chrome_adapters::read_raw(&name).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub struct SaveAdapterArgs {
    pub name: String,
    pub yaml: String,
}

#[tauri::command]
pub fn chrome_adapter_save(args: SaveAdapterArgs) -> Result<String, String> {
    let p = glance::install::chrome_adapters::save_raw(&args.name, &args.yaml)
        .map_err(|e| e.to_string())?;
    Ok(p.display().to_string())
}

#[tauri::command]
pub fn chrome_adapter_delete(name: String) -> Result<(), String> {
    glance::install::chrome_adapters::delete(&name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn chrome_adapter_open_dir() -> Result<(), String> {
    let dir = glance::install::chrome_adapters::adapters_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("open")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── "Save last evaluate as adapter" auto-capture ────────────────────────────
//
// The `chrome` tool stashes the most recent successful `evaluate` per tab in
// memory. The GUI uses this command to turn that captured expression into a
// named YAML adapter without retyping. Cache lives in glance-mcp's process —
// when glance-mcp restarts, the slate empties.

#[derive(Deserialize)]
pub struct SaveLastEvaluateArgs {
    pub tab_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub match_url: Option<String>,
}

/// Returned shape of the captured-evaluate snapshot — used by the GUI to
/// preview what would be saved before the user commits to a name/description.
#[derive(Serialize)]
pub struct LastEvaluatePreview {
    pub tab_id: i64,
    pub expression: String,
    pub await_promise: bool,
    pub ts: u64,
    pub tab_url: String,
}

#[tauri::command]
pub fn chrome_get_last_evaluate(tab_id: i64) -> Result<Option<LastEvaluatePreview>, String> {
    Ok(glance::tools::chrome::get_last_evaluate(tab_id).map(|r| LastEvaluatePreview {
        tab_id,
        expression: r.expression,
        await_promise: r.await_promise,
        ts: r.ts,
        tab_url: r.tab_url,
    }))
}

#[tauri::command]
pub fn chrome_save_last_evaluate_as_adapter(
    args: SaveLastEvaluateArgs,
) -> Result<String, String> {
    glance::install::chrome_adapters::validate_name(&args.name).map_err(|e| e.to_string())?;
    let rec = glance::tools::chrome::get_last_evaluate(args.tab_id).ok_or_else(|| {
        format!(
            "no captured evaluate for tab_id {} — run an `evaluate` action that returns a value first",
            args.tab_id
        )
    })?;
    let match_url = match args.match_url.as_deref() {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => glance::install::chrome_adapters::derive_match_url_from(&rec.tab_url),
    };
    let adapter = glance::install::chrome_adapters::Adapter {
        name: args.name,
        description: args.description.filter(|s| !s.trim().is_empty()),
        match_url: Some(match_url),
        args: vec![],
        evaluate: rec.expression,
        await_promise: rec.await_promise,
        world: None,
        source_path: None,
    };
    let path = glance::install::chrome_adapters::save(&adapter).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

// ── Per-tool client allowlist ───────────────────────────────────────────────
//
// Mirrors `UpstreamMcp.clients`: empty / missing list = visible to every
// MCP client (claude / codex / cursor); non-empty = only those clients see
// the tool in `tools/list`.

#[derive(Serialize)]
pub struct ToolClientsEntry {
    pub key: String,
    pub clients: Vec<String>,
}

#[tauri::command]
pub fn list_tool_clients() -> Result<Vec<ToolClientsEntry>, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    Ok(cfg
        .tools_clients
        .into_iter()
        .map(|(key, clients)| ToolClientsEntry { key, clients })
        .collect())
}

#[derive(Deserialize)]
pub struct SetToolClientsArgs {
    pub name: String,
    pub clients: Vec<String>,
}

#[tauri::command]
pub fn set_tool_clients(args: SetToolClientsArgs) -> Result<(), String> {
    let mut cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    // Normalize: lower-case, dedupe; ignore unknown client ids silently.
    let known = ["claude", "codex", "cursor"];
    let mut filtered: Vec<String> = args
        .clients
        .iter()
        .map(|c| c.to_lowercase())
        .filter(|c| known.contains(&c.as_str()))
        .collect();
    filtered.sort();
    filtered.dedup();
    if filtered.len() == known.len() || filtered.is_empty() {
        // All clients allowed (or no filter at all) → drop the entry so the
        // config stays minimal and back-compat with consumers.
        cfg.tools_clients.remove(&args.name);
    } else {
        cfg.tools_clients.insert(args.name, filtered);
    }
    save_config(&cfg)
}

// ── Test connection ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct BackendCheck {
    pub ok: bool,
    pub status: u16,
    pub model: String,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn test_backend() -> Result<BackendCheck, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let url = format!(
        "{}/chat/completions",
        cfg.backend.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "model": cfg.backend.model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });
    let started = Instant::now();
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.backend.api_key)
        .json(&body)
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let ok = r.status().is_success();
            let error = if ok {
                None
            } else {
                let text = r.text().await.unwrap_or_default();
                let head: String = text.chars().take(200).collect();
                Some(format!("HTTP {}: {}", status, head))
            };
            Ok(BackendCheck {
                ok,
                status,
                model: cfg.backend.model,
                latency_ms,
                error,
            })
        }
        Err(e) => Ok(BackendCheck {
            ok: false,
            status: 0,
            model: cfg.backend.model,
            latency_ms,
            error: Some(e.to_string()),
        }),
    }
}

/// GET {base_url}/models — return the model id list. OpenAI / GLM / DeepSeek
/// all expose this. Fails cleanly so the GUI can fall back to free-text input.
#[tauri::command]
pub async fn list_models() -> Result<Vec<String>, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let url = format!("{}/models", cfg.backend.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .bearer_auth(&cfg.backend.api_key)
        .send()
        .await
        .map_err(|e| format!("list_models GET: {}", e))?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "list_models {} → {}: {}",
            url,
            st,
            body.chars().take(200).collect::<String>()
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("list_models JSON decode: {}", e))?;
    // OpenAI shape: {"data":[{"id":"..."}]}; some servers return a bare array.
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| v.as_array())
        .ok_or_else(|| "list_models: response had no models array".to_string())?;
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[derive(Serialize)]
pub struct PingResult {
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Ping a SPECIFIC model (not the configured default) with a 1-token request.
/// Used by the Backend tab to validate fallback_models entries before they
/// matter in production.
#[tauri::command]
pub async fn ping_model(model: String) -> Result<PingResult, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    if model.trim().is_empty() {
        return Err("ping_model: model is empty".to_string());
    }
    let url = format!(
        "{}/chat/completions",
        cfg.backend.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });
    let started = Instant::now();
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.backend.api_key)
        .json(&body)
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let ok = r.status().is_success();
            let error = if ok {
                None
            } else {
                Some(
                    r.text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>(),
                )
            };
            Ok(PingResult {
                ok,
                status,
                latency_ms,
                error,
            })
        }
        Err(e) => Ok(PingResult {
            ok: false,
            status: 0,
            latency_ms,
            error: Some(e.to_string()),
        }),
    }
}

/// Validate `tokens.github` (or `GITHUB_TOKEN` env) by calling the GitHub
/// `/user` endpoint. Returns username + scopes on success — those tell us
/// the token is real AND has the right permissions for `repo_explore`.
#[derive(Serialize)]
pub struct GitHubTokenCheck {
    pub ok: bool,
    pub status: u16,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn test_github_token() -> Result<GitHubTokenCheck, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let Some(token) = cfg.tokens.resolved_github() else {
        return Ok(GitHubTokenCheck {
            ok: false,
            status: 0,
            login: None,
            scopes: Vec::new(),
            latency_ms: 0,
            error: Some("no GitHub token set (tokens.github / GITHUB_TOKEN)".to_string()),
        });
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("glance-app")
        .build()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let resp = client
        .get("https://api.github.com/user")
        .bearer_auth(&token)
        .header("accept", "application/vnd.github+json")
        .send()
        .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let ok = r.status().is_success();
            let scopes = r
                .headers()
                .get("x-oauth-scopes")
                .and_then(|v| v.to_str().ok())
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if ok {
                let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
                let login = body.get("login").and_then(|x| x.as_str()).map(String::from);
                Ok(GitHubTokenCheck {
                    ok: true,
                    status,
                    login,
                    scopes,
                    latency_ms,
                    error: None,
                })
            } else {
                let body = r.text().await.unwrap_or_default();
                Ok(GitHubTokenCheck {
                    ok: false,
                    status,
                    login: None,
                    scopes: Vec::new(),
                    latency_ms,
                    error: Some(body.chars().take(200).collect::<String>()),
                })
            }
        }
        Err(e) => Ok(GitHubTokenCheck {
            ok: false,
            status: 0,
            login: None,
            scopes: Vec::new(),
            latency_ms,
            error: Some(e.to_string()),
        }),
    }
}

// ── events.jsonl helpers ────────────────────────────────────────────────────

fn events_file_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".glance").join("events.jsonl"))
}

#[derive(Serialize)]
pub struct EventLine {
    pub ts: String,
    pub tool: String,
    pub duration_ms: u64,
    pub ok: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub savings_pct: f64,
    /// Total GLM tokens (prompt + completion) burned by this tool call's
    /// internal sub-agent loops. 0 for tools that don't drive a sub-agent
    /// (md_outline, image_describe pre-vision-call, etc.) and for legacy
    /// rows recorded before the field existed.
    #[serde(default)]
    pub glm_tokens: u32,
    #[serde(default)]
    pub glm_prompt_tokens: u32,
    #[serde(default)]
    pub glm_completion_tokens: u32,
    /// Prompt tokens served from the backend's prefix cache (subset of
    /// `glm_prompt_tokens`). Anthropic/OpenAI/DeepSeek all report this in
    /// different shapes; Glance normalises them. cache_hit_rate =
    /// glm_cached_tokens / glm_prompt_tokens.
    #[serde(default)]
    pub glm_cached_tokens: u32,
    /// Anthropic-only: prompt tokens billed at cache-WRITE rate (1.25×).
    #[serde(default)]
    pub glm_cache_creation_tokens: u32,
    #[serde(default)]
    pub iters: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl EventLine {
    fn from_tool_event(e: &ToolEvent) -> Self {
        let savings_pct = if e.bytes_in == 0 {
            0.0
        } else {
            let b_in = e.bytes_in as f64;
            let b_out = e.bytes_out as f64;
            ((b_in - b_out) / b_in) * 100.0
        };
        Self {
            ts: e.ts.clone(),
            tool: e.tool.clone(),
            duration_ms: e.duration_ms,
            ok: e.ok,
            bytes_in: e.bytes_in,
            bytes_out: e.bytes_out,
            savings_pct,
            glm_tokens: e.tokens,
            glm_prompt_tokens: e.glm_prompt_tokens,
            glm_completion_tokens: e.glm_completion_tokens,
            glm_cached_tokens: e.glm_cached_tokens,
            glm_cache_creation_tokens: e.glm_cache_creation_tokens,
            iters: e.iters,
            error: e.error.clone(),
        }
    }
}

#[tauri::command]
pub fn tail_events(n: u32) -> Result<Vec<EventLine>, String> {
    let path = events_file_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = text.lines().collect();
    let n = n as usize;
    let start = lines.len().saturating_sub(n);
    let mut out = Vec::with_capacity(lines.len() - start);
    for line in &lines[start..] {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<ToolEvent>(line) {
            out.push(EventLine::from_tool_event(&e));
        }
    }
    Ok(out)
}

#[tauri::command]
pub fn clear_events() -> Result<(), String> {
    let path = events_file_path().map_err(|e| e.to_string())?;
    if path.exists() {
        std::fs::write(&path, "").map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── today stats ─────────────────────────────────────────────────────────────

#[derive(Serialize, Default)]
pub struct TodayStats {
    pub calls: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub savings_pct: f64,
    pub ok_count: u64,
    pub err_count: u64,
    /// Total GLM tokens (prompt + completion) consumed today across every
    /// recorded tool call. Sums `tokens` from each event line.
    pub glm_total_tokens: u64,
    /// Average GLM tokens per call, computed only over calls that actually
    /// drove a sub-agent (`tokens > 0`). Skipping the zeros prevents
    /// md_outline / cache hits from artificially deflating the average.
    pub glm_avg_per_call: u64,
    /// Number of calls whose `tokens > 0` — i.e. the denominator of
    /// `glm_avg_per_call`. Useful sub-label for the Status tile.
    pub glm_billed_calls: u64,
}

#[tauri::command]
pub fn today_stats() -> Result<TodayStats, String> {
    let path = events_file_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Ok(TodayStats::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let today_prefix = Utc::now().format("%Y-%m-%d").to_string();

    let mut stats = TodayStats::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<ToolEvent>(line) else {
            continue;
        };
        // ts is ISO-8601 (RFC3339); cheap prefix match avoids parsing fees.
        let is_today = e.ts.starts_with(&today_prefix)
            || DateTime::parse_from_rfc3339(&e.ts)
                .map(|dt| dt.with_timezone(&Utc).format("%Y-%m-%d").to_string() == today_prefix)
                .unwrap_or(false);
        if !is_today {
            continue;
        }
        stats.calls += 1;
        stats.bytes_in += e.bytes_in;
        stats.bytes_out += e.bytes_out;
        if e.tokens > 0 {
            stats.glm_total_tokens += e.tokens as u64;
            stats.glm_billed_calls += 1;
        }
        if e.ok {
            stats.ok_count += 1;
        } else {
            stats.err_count += 1;
        }
    }
    stats.savings_pct = if stats.bytes_in == 0 {
        0.0
    } else {
        let b_in = stats.bytes_in as f64;
        let b_out = stats.bytes_out as f64;
        ((b_in - b_out) / b_in) * 100.0
    };
    stats.glm_avg_per_call = if stats.glm_billed_calls == 0 {
        0
    } else {
        stats.glm_total_tokens / stats.glm_billed_calls
    };
    Ok(stats)
}

// ── folder picker ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[allow(dead_code)]
struct PickFolderArgs {
    title: Option<String>,
}

#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Select a folder")
        .pick_folder(move |path| {
            let _ = tx.send(path);
        });
    let picked = rx.await.map_err(|e| e.to_string())?;
    Ok(picked.map(|p| p.to_string()))
}

// ── url opener ──────────────────────────────────────────────────────────────

#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

// ── window helpers ──────────────────────────────────────────────────────────

#[tauri::command]
pub fn show_main_window_cmd(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        #[cfg(target_os = "macos")]
        {
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
        }
    }
    Ok(())
}

// ── Upstream MCP aggregator ─────────────────────────────────────────────────
//
// The aggregator is owned by glance-mcp (the stdio binary spawned by codex /
// claude / cursor). The GUI manages the *config* — every add/remove writes
// `~/.glance/config.toml` and rebuilds the in-process aggregator the GUI also
// holds (so `test` / status reflects the current spec). Glance-mcp picks up
// the new config the next time it's restarted by its host (auto on next
// `tools/list` if Claude reconnects, otherwise explicit reload).

/// Helper: write the full config back to disk.
fn save_config(cfg: &Config) -> Result<(), String> {
    let path = user_config_path().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let toml = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, toml).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn list_upstream_mcps() -> Result<Vec<UpstreamMcpListEntry>, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    // Make sure we have a current aggregator snapshot, rebuilding lazily if
    // the GUI hasn't done it yet (e.g. first open after launch).
    if mcp_aggregator::current().await.is_none() {
        let _ = mcp_aggregator::rebuild_from_config().await;
    }
    let snap: Vec<UpstreamStatusSnapshot> = match mcp_aggregator::current().await {
        Some(agg) => agg.status_snapshot().await,
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(cfg.upstream_mcps.len());
    for spec in &cfg.upstream_mcps {
        let runtime = snap.iter().find(|s| s.name == spec.name()).cloned();
        out.push(UpstreamMcpListEntry {
            spec: spec.clone(),
            runtime,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct UpstreamMcpListEntry {
    pub spec: UpstreamMcp,
    /// Runtime state (status, tool count, last error). `None` if the
    /// aggregator hasn't tried this entry yet (e.g. just added).
    pub runtime: Option<UpstreamStatusSnapshot>,
}

/// Add OR update an upstream by name (upsert). Used by the "Add new" form
/// AND by every per-card edit path in the GUI (chip toggles, enable/disable
/// — they all rewrite the spec by name, so rejecting duplicates would
/// silently fail those edits).
#[tauri::command]
pub async fn add_upstream_mcp(spec: UpstreamMcp) -> Result<(), String> {
    let mut cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let name = spec.name().to_string();
    if name.is_empty() {
        return Err("upstream name is required".into());
    }
    if name.contains("__") {
        return Err("upstream name must not contain '__' (used as namespace separator)".into());
    }
    let existing = cfg.upstream_mcps.iter().position(|s| s.name() == name);
    match existing {
        Some(idx) => cfg.upstream_mcps[idx] = spec, // update in place
        None => cfg.upstream_mcps.push(spec),       // append new
    }
    save_config(&cfg)?;
    mcp_aggregator::rebuild_from_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn remove_upstream_mcp(name: String) -> Result<(), String> {
    let mut cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let before = cfg.upstream_mcps.len();
    cfg.upstream_mcps.retain(|s| s.name() != name);
    if cfg.upstream_mcps.len() == before {
        return Err(format!("upstream `{}` not found", name));
    }
    save_config(&cfg)?;
    mcp_aggregator::rebuild_from_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Deserialize)]
pub struct ToggleArgs {
    pub name: String,
    pub enabled: bool,
}

#[tauri::command]
pub async fn set_upstream_mcp_enabled(args: ToggleArgs) -> Result<(), String> {
    let mut cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    let mut hit = false;
    for spec in cfg.upstream_mcps.iter_mut() {
        if spec.name() == args.name {
            *spec = match spec.clone() {
                UpstreamMcp::Stdio {
                    name,
                    command,
                    args: a,
                    env,
                    clients,
                    prelude_call,
                    ..
                } => UpstreamMcp::Stdio {
                    name,
                    command,
                    args: a,
                    env,
                    enabled: args.enabled,
                    clients,
                    prelude_call,
                },
                UpstreamMcp::StreamableHttp {
                    name,
                    url,
                    api_key,
                    clients,
                    prelude_call,
                    ..
                } => UpstreamMcp::StreamableHttp {
                    name,
                    url,
                    api_key,
                    enabled: args.enabled,
                    clients,
                    prelude_call,
                },
            };
            hit = true;
            break;
        }
    }
    if !hit {
        return Err(format!("upstream `{}` not found", args.name));
    }
    save_config(&cfg)?;
    mcp_aggregator::rebuild_from_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn test_upstream_mcp(spec: UpstreamMcp) -> Result<SmokeTestResult, String> {
    let cfg = glance::config::load_or_default().map_err(|e| e.to_string())?;
    Ok(mcp_aggregator::smoke_test(spec, &cfg.backend.api_key).await)
}

#[tauri::command]
pub fn list_upstream_templates() -> Result<Vec<UpstreamTemplate>, String> {
    Ok(mcp_aggregator::list_templates())
}

/// Force a fresh aggregator rebuild — useful from the GUI after the user
/// edits config externally.
#[tauri::command]
pub async fn reload_upstream_mcps() -> Result<Vec<UpstreamStatusSnapshot>, String> {
    mcp_aggregator::rebuild_from_config()
        .await
        .map_err(|e| e.to_string())?;
    let snap = match mcp_aggregator::current().await {
        Some(a) => a.status_snapshot().await,
        None => Vec::new(),
    };
    Ok(snap)
}

// ── rtk (rtk-ai/rtk) integration ────────────────────────────────────────────
//
// rtk is a separate CLI binary (`/opt/homebrew/bin/rtk`, Rust, MIT) that
// intercepts Bash-tool calls in Claude Code / codex / cursor at the hook
// layer, rewrites commands like `git status` → `rtk git status`, and filters
// the output before it reaches the agent's context. The numbers it tracks
// (`rtk gain`) are the canonical source for "tokens saved" stats this tab
// surfaces — we don't double-count from glance's own events.jsonl.
//
// Three flavors of integration on disk:
//   - claude:  hook in ~/.claude/settings.json + RTK.md reference
//   - codex:   AGENTS.md @RTK.md mention (codex CLI has no hook protocol)
//   - cursor:  hook via `rtk init -g --agent cursor`
//
// Stats live in ~/Library/Application Support/rtk/history.db (SQLite). We
// shell out to the system `sqlite3` for history rows (cheaper than adding
// rusqlite + libsqlite3 to the Tauri bundle) and to `rtk gain --format json`
// for the rolled-up summary.

const RTK_BIN: &str = "/opt/homebrew/bin/rtk";

fn rtk_bin_path() -> String {
    // Allow override for tests / non-mac dev. Falls back to PATH lookup
    // before the homebrew default — keeps things working on Linux too.
    if let Ok(env_bin) = std::env::var("GLANCE_RTK_BIN") {
        if !env_bin.is_empty() {
            return env_bin;
        }
    }
    if std::path::Path::new(RTK_BIN).exists() {
        RTK_BIN.to_string()
    } else {
        "rtk".to_string()
    }
}

#[derive(Serialize, Default)]
pub struct RtkStatus {
    pub installed: bool,
    pub binary_path: Option<String>,
    pub version: Option<String>,
    pub claude_hook: bool,
    pub codex_agents_md: bool,
    pub cursor_hook: bool,
}

#[derive(Serialize, Default, Deserialize)]
pub struct RtkGain {
    pub total_commands: u64,
    pub total_input: u64,
    pub total_output: u64,
    pub total_saved: u64,
    pub avg_savings_pct: f64,
    pub total_time_ms: u64,
    pub avg_time_ms: u64,
}

#[derive(Serialize)]
pub struct RtkHistoryEntry {
    pub timestamp: String,
    pub command: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub savings_pct: f64,
    pub time_ms: u64,
}

/// Run `rtk <args>` with a hard 10s ceiling. Returns (stdout, stderr, success).
async fn run_rtk(args: &[&str]) -> Result<(String, String, bool), String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    let bin = rtk_bin_path();
    let fut = Command::new(&bin).args(args).output();
    let res = timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| format!("rtk {:?}: timeout after 10s", args))?
        .map_err(|e| format!("rtk {:?}: spawn failed: {}", args, e))?;
    Ok((
        String::from_utf8_lossy(&res.stdout).to_string(),
        String::from_utf8_lossy(&res.stderr).to_string(),
        res.status.success(),
    ))
}

/// Parse `rtk init --show` text output for "[ok] settings.json: RTK hook
/// configured" → claude_hook=true, "[--] Cursor hook: not found" → false.
fn parse_show_for_claude_hook(text: &str) -> bool {
    text.lines()
        .any(|l| l.contains("[ok]") && l.to_lowercase().contains("settings.json"))
}

fn parse_show_for_cursor_hook(text: &str) -> bool {
    text.lines().any(|l| {
        let lower = l.to_lowercase();
        lower.contains("cursor") && lower.contains("hook") && l.contains("[ok]")
    })
}

/// Parse `rtk init --show --codex` for "[ok] Global AGENTS.md: RTK.md reference".
fn parse_show_for_codex(text: &str) -> bool {
    text.lines().any(|l| {
        l.contains("[ok]")
            && (l.contains("AGENTS.md") || l.contains("agents.md"))
            && l.to_lowercase().contains("rtk")
    })
}

#[tauri::command]
pub async fn rtk_status() -> Result<RtkStatus, String> {
    let bin = rtk_bin_path();
    let exists = std::path::Path::new(&bin).exists() || which_in_path(&bin).is_some();
    if !exists {
        return Ok(RtkStatus {
            installed: false,
            binary_path: None,
            version: None,
            claude_hook: false,
            codex_agents_md: false,
            cursor_hook: false,
        });
    }

    let version = match run_rtk(&["--version"]).await {
        Ok((stdout, _, true)) => stdout
            .split_whitespace()
            .nth(1)
            .map(|s| s.trim().to_string()),
        _ => None,
    };

    // Claude and cursor surfaces share the same `init --show` output (the
    // tool reports cursor presence on the same default report).
    let (claude_hook, cursor_hook) = match run_rtk(&["init", "--show"]).await {
        Ok((stdout, _, _)) => (
            parse_show_for_claude_hook(&stdout),
            parse_show_for_cursor_hook(&stdout),
        ),
        Err(_) => (false, false),
    };

    let codex_agents_md = match run_rtk(&["init", "--show", "--codex"]).await {
        Ok((stdout, _, _)) => parse_show_for_codex(&stdout),
        Err(_) => false,
    };

    Ok(RtkStatus {
        installed: true,
        binary_path: Some(bin),
        version,
        claude_hook,
        codex_agents_md,
        cursor_hook,
    })
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `rtk gain --format json` → `{"summary": {...}}`. Empty when no commands
/// have run yet (rtk prints "No tracking data yet." on text path; the JSON
/// path returns `{"summary": {... zeros ...}}` once the DB exists).
#[tauri::command]
pub async fn rtk_gain() -> Result<RtkGain, String> {
    let (stdout, stderr, ok) = run_rtk(&["gain", "--format", "json"]).await?;
    if !ok {
        // Empty DB / never-fired path: return zeros instead of a hard error
        // so the GUI can show "No tracking data yet."
        if stderr.contains("No tracking data") || stdout.contains("No tracking data") {
            return Ok(RtkGain::default());
        }
        return Err(format!("rtk gain failed: {}", stderr.trim()));
    }
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "rtk gain: invalid JSON ({}): {}",
            e,
            stdout.chars().take(200).collect::<String>()
        )
    })?;
    let summary = v
        .get("summary")
        .ok_or_else(|| "rtk gain: response missing `summary`".to_string())?;
    serde_json::from_value(summary.clone()).map_err(|e| format!("rtk gain: summary decode: {}", e))
}

/// History rows live in `~/Library/Application Support/rtk/history.db`
/// (sqlite). `rtk gain --history --format json` does NOT include rows
/// (verified: it returns the summary block only). Easiest source of truth
/// is the DB itself, queried via the system `sqlite3` shell — adding the
/// rusqlite crate would pull a C dep into the Tauri bundle for one query.
#[tauri::command]
pub async fn rtk_history(limit: u32) -> Result<Vec<RtkHistoryEntry>, String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    let db = rtk_history_db_path().ok_or_else(|| "no rtk data dir".to_string())?;
    if !db.exists() {
        return Ok(Vec::new());
    }
    // Cap upstream callers — 1000 rows is more than the table's natural
    // working set and keeps the IPC payload tiny.
    let n = limit.clamp(1, 1000);
    let sql = format!(
        "SELECT timestamp, original_cmd, input_tokens, output_tokens, savings_pct, exec_time_ms \
         FROM commands ORDER BY id DESC LIMIT {};",
        n
    );
    let fut = Command::new("sqlite3")
        .arg("-separator")
        // ASCII unit separator — won't appear in any sane shell command.
        .arg("\u{1f}")
        .arg(db.as_os_str())
        .arg(&sql)
        .output();
    let out = timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| "sqlite3 history: timeout after 10s".to_string())?
        .map_err(|e| format!("sqlite3 history: spawn failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3 history: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::with_capacity(n as usize);
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\u{1f}').collect();
        if parts.len() < 6 {
            continue;
        }
        rows.push(RtkHistoryEntry {
            timestamp: parts[0].to_string(),
            command: parts[1].to_string(),
            input_bytes: parts[2].parse().unwrap_or(0),
            output_bytes: parts[3].parse().unwrap_or(0),
            savings_pct: parts[4].parse().unwrap_or(0.0),
            time_ms: parts[5].parse().unwrap_or(0),
        });
    }
    Ok(rows)
}

fn rtk_history_db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join("Library")
            .join("Application Support")
            .join("rtk")
            .join("history.db"),
    )
}

/// Install (or reinstall) rtk for the given client. Captures stdout so the
/// GUI can echo the install summary. Per-client argv shape:
///   - "claude":  rtk init -g --auto-patch
///   - "codex":   rtk init -g --codex     (no --auto-patch — incompatible)
///   - "cursor":  rtk init -g --agent cursor
#[tauri::command]
pub async fn rtk_init(client: String) -> Result<String, String> {
    let args: Vec<&str> = match client.as_str() {
        "claude" => vec!["init", "-g", "--auto-patch"],
        "codex" => vec!["init", "-g", "--codex"],
        "cursor" => vec!["init", "-g", "--agent", "cursor"],
        other => return Err(format!("unknown rtk client: {}", other)),
    };
    let (stdout, stderr, ok) = run_rtk(&args).await?;
    if !ok {
        return Err(format!("rtk init {} failed: {}", client, stderr.trim()));
    }
    Ok(stdout.trim().to_string())
}

#[tauri::command]
pub async fn rtk_uninstall(client: String) -> Result<String, String> {
    let args: Vec<&str> = match client.as_str() {
        "claude" => vec!["init", "-g", "--uninstall"],
        "codex" => vec!["init", "-g", "--codex", "--uninstall"],
        "cursor" => vec!["init", "-g", "--agent", "cursor", "--uninstall"],
        other => return Err(format!("unknown rtk client: {}", other)),
    };
    let (stdout, stderr, ok) = run_rtk(&args).await?;
    if !ok {
        return Err(format!(
            "rtk uninstall {} failed: {}",
            client,
            stderr.trim()
        ));
    }
    Ok(stdout.trim().to_string())
}

#[derive(Serialize)]
pub struct RtkUpdateCheck {
    pub current: Option<String>,
    pub latest: Option<String>,
    pub outdated: bool,
    pub source: &'static str, // "github-releases"
    pub error: Option<String>,
}

/// Compare current `rtk --version` against the latest GitHub release tag.
/// Source of truth = `https://api.github.com/repos/rtk-ai/rtk/releases/latest`.
/// Returns `outdated: false` when versions match OR when either side is
/// missing (don't bug the user with a false update prompt on transient
/// network errors).
#[tauri::command]
pub async fn rtk_check_update() -> Result<RtkUpdateCheck, String> {
    // Local: parse `rtk --version` → "rtk 0.39.0" → "0.39.0".
    let current = run_rtk(&["--version"])
        .await
        .ok()
        .filter(|(_, _, ok)| *ok)
        .and_then(|(stdout, _, _)| {
            stdout
                .trim()
                .strip_prefix("rtk ")
                .map(|s| s.split_whitespace().next().unwrap_or(s).to_string())
        });

    // Remote: GitHub releases API. No auth needed for public repo (60/h
    // anonymous limit is plenty for one update check).
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("glance-app")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get("https://api.github.com/repos/rtk-ai/rtk/releases/latest")
        .header("accept", "application/vnd.github+json")
        .send()
        .await;
    let latest = match resp {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("tag_name").and_then(|t| t.as_str()).map(String::from))
            .map(|t| t.trim_start_matches('v').to_string()),
        _ => None,
    };

    let outdated = match (current.as_deref(), latest.as_deref()) {
        (Some(c), Some(l)) => semver_lt(c, l),
        _ => false,
    };

    Ok(RtkUpdateCheck {
        current,
        latest,
        outdated,
        source: "github-releases",
        error: None,
    })
}

/// Cheap semver comparator: split on '.', compare numeric components in
/// order. Handles 0.9.0 < 0.10.0 correctly (lexicographic doesn't).
fn semver_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.split(|c: char| !c.is_ascii_digit() && c != '.')
            .next()
            .unwrap_or(s)
            .split('.')
            .map(|n| n.parse::<u32>().unwrap_or(0))
            .collect()
    };
    let av = parse(a);
    let bv = parse(b);
    let len = av.len().max(bv.len());
    for i in 0..len {
        let ai = av.get(i).copied().unwrap_or(0);
        let bi = bv.get(i).copied().unwrap_or(0);
        if ai < bi {
            return true;
        }
        if ai > bi {
            return false;
        }
    }
    false
}

#[derive(Serialize)]
pub struct RtkUpdateResult {
    pub ok: bool,
    pub method: String, // "brew" | "cargo" | "curl"
    pub stdout: String,
    pub stderr: String,
}

/// Run `brew upgrade rtk` (preferred) or `cargo install --git ...` as
/// fallback. Returns the captured stdout/stderr.
#[tauri::command]
pub async fn rtk_update() -> Result<RtkUpdateResult, String> {
    // Prefer brew if rtk was installed that way (binary path under /opt/homebrew or /usr/local/Cellar).
    let brew_owned = run_cmd("brew", &["list", "rtk"])
        .await
        .map(|(_, _, ok)| ok)
        .unwrap_or(false);
    if brew_owned {
        let (stdout, stderr, ok) = run_cmd("brew", &["upgrade", "rtk"]).await?;
        return Ok(RtkUpdateResult {
            ok,
            method: "brew".into(),
            stdout,
            stderr,
        });
    }
    // Fallback: cargo install --git (overwrites in-place at ~/.cargo/bin/rtk).
    let (stdout, stderr, ok) = run_cmd(
        "cargo",
        &[
            "install",
            "--git",
            "https://github.com/rtk-ai/rtk",
            "--force",
        ],
    )
    .await?;
    Ok(RtkUpdateResult {
        ok,
        method: "cargo".into(),
        stdout,
        stderr,
    })
}

async fn run_cmd(bin: &str, args: &[&str]) -> Result<(String, String, bool), String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("spawn {}: {}", bin, e))?;
    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    ))
}

// ── 08 CCUSAGE — ryoppippi/ccusage shell-out ────────────────────────────────
//
// `npx -y ccusage@latest <subcmd> --json` produces token / cost reports
// scanned from `~/.claude/projects/**/*.jsonl` (Claude Code) and
// `~/.codex/sessions/**/*.jsonl` (Codex CLI).  ccusage doesn't itself
// distinguish "claude vs codex" — we tag each row by inspecting `modelsUsed`
// (any `claude-*` model → claude; everything else, eg. `glm-*`/`gpt-*`,
// belongs to a Codex-style session).
//
// Caveat: under some user npm configs, npx leaks `npm warn ...` lines onto
// the same stdout as the JSON, breaking strict `serde_json::from_str`. We
// extract the first balanced `{...}` JSON object before parsing.

#[derive(Serialize)]
pub struct CcusageStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub claude_jsonl_count: u64,
    pub codex_jsonl_count: u64,
    pub error: Option<String>,
}

#[derive(Serialize, Default)]
pub struct CcusageDailyEntry {
    pub date: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub models_used: Vec<String>,
    /// "claude" | "codex" | "mixed" | "none"
    pub source: &'static str,
}

#[derive(Serialize)]
pub struct CcusageDailyResponse {
    pub entries: Vec<CcusageDailyEntry>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
}

#[derive(Serialize, Default)]
pub struct CcusageSessionEntry {
    pub session_id: String,
    pub last_activity: String,
    pub project: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub models_used: Vec<String>,
    pub source: &'static str,
}

/// Slice the first balanced JSON object (or array) out of a stdout buffer
/// so trailing `npm warn ...` lines don't poison serde_json. Tracks string
/// state so braces inside string literals are ignored.
fn extract_json_blob(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escape = false;
    let mut opener: u8 = b'{';
    for (i, &b) in bytes.iter().enumerate() {
        if start.is_none() {
            if b == b'{' || b == b'[' {
                start = Some(i);
                depth = 1;
                opener = b;
                continue;
            }
            continue;
        }
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    let st = start.unwrap();
                    // Sanity: matching closer for the opener type.
                    let close_match =
                        (opener == b'{' && b == b'}') || (opener == b'[' && b == b']');
                    if close_match {
                        return Some(&s[st..=i]);
                    }
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

fn classify_source(models: &[String]) -> &'static str {
    if models.is_empty() {
        return "none";
    }
    let mut has_claude = false;
    let mut has_other = false;
    for m in models {
        let lower = m.to_lowercase();
        if lower.starts_with("claude") {
            has_claude = true;
        } else if !lower.is_empty() {
            has_other = true;
        }
    }
    match (has_claude, has_other) {
        (true, false) => "claude",
        (false, true) => "codex",
        (true, true) => "mixed",
        _ => "none",
    }
}

/// Run `npx -y ccusage@latest <args>` with a 60s ceiling.
async fn run_ccusage(args: &[&str]) -> Result<(String, String, bool), String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    let mut full = vec!["-y", "ccusage@latest"];
    full.extend_from_slice(args);
    let fut = Command::new("npx").args(&full).output();
    let res = timeout(Duration::from_secs(60), fut)
        .await
        .map_err(|_| format!("npx ccusage {:?}: timeout after 60s", args))?
        .map_err(|e| format!("npx ccusage {:?}: spawn failed: {}", args, e))?;
    Ok((
        String::from_utf8_lossy(&res.stdout).to_string(),
        String::from_utf8_lossy(&res.stderr).to_string(),
        res.status.success(),
    ))
}

fn count_jsonl_in(dir: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, out: &mut u64, depth: usize) {
        if depth > 6 {
            return;
        }
        let Ok(read) = std::fs::read_dir(p) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&path, out, depth + 1);
            } else if ft.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                *out += 1;
            }
        }
    }
    let mut n = 0u64;
    if dir.exists() {
        walk(dir, &mut n, 0);
    }
    n
}

#[tauri::command]
pub async fn ccusage_status() -> Result<CcusageStatus, String> {
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    let claude_count = count_jsonl_in(&home.join(".claude").join("projects"));
    let codex_count = count_jsonl_in(&home.join(".codex").join("sessions"));

    // `npx -y ccusage@latest --version` resolves the package even when not
    // installed globally — it triggers npx's first-run install which is the
    // exact mechanism the runtime commands rely on. A short timeout keeps the
    // tab snappy; if the cache is cold the first call may take >5s.
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};
    let fut = Command::new("npx")
        .args(["-y", "ccusage@latest", "--version"])
        .output();
    match timeout(Duration::from_secs(45), fut).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let version = stdout
                .lines()
                .find(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with("npm ")
                })
                .map(|l| l.trim().to_string());
            Ok(CcusageStatus {
                installed: out.status.success(),
                version,
                claude_jsonl_count: claude_count,
                codex_jsonl_count: codex_count,
                error: if out.status.success() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&out.stderr).trim().to_string())
                },
            })
        }
        Ok(Err(e)) => Ok(CcusageStatus {
            installed: false,
            version: None,
            claude_jsonl_count: claude_count,
            codex_jsonl_count: codex_count,
            error: Some(format!("npx spawn failed: {}", e)),
        }),
        Err(_) => Ok(CcusageStatus {
            installed: false,
            version: None,
            claude_jsonl_count: claude_count,
            codex_jsonl_count: codex_count,
            error: Some("npx ccusage --version: timeout after 45s".into()),
        }),
    }
}

/// Parse the JSON returned by `ccusage daily --json`. The second arg is the
/// max number of (most-recent) days to keep.
pub(crate) fn parse_daily_json(raw: &str, days: u32) -> Result<CcusageDailyResponse, String> {
    let blob = extract_json_blob(raw)
        .ok_or_else(|| "ccusage daily: no JSON object found in stdout".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(blob).map_err(|e| format!("ccusage daily: invalid JSON: {}", e))?;
    let arr = v
        .get("daily")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "ccusage daily: missing `daily` array".to_string())?;
    let totals = v.get("totals");
    let mut entries: Vec<CcusageDailyEntry> = arr
        .iter()
        .map(|item| {
            let models_used: Vec<String> = item
                .get("modelsUsed")
                .and_then(|x| x.as_array())
                .map(|xs| {
                    xs.iter()
                        .filter_map(|m| m.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let source = classify_source(&models_used);
            CcusageDailyEntry {
                date: item
                    .get("date")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_tokens: item
                    .get("inputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                output_tokens: item
                    .get("outputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cache_creation_tokens: item
                    .get("cacheCreationTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cache_read_tokens: item
                    .get("cacheReadTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                total_tokens: item
                    .get("totalTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                estimated_cost_usd: item
                    .get("totalCost")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0),
                models_used,
                source,
            }
        })
        .collect();

    // ccusage emits ascending dates; keep last `days`, then return descending.
    if days > 0 && entries.len() > days as usize {
        let drop = entries.len() - days as usize;
        entries.drain(0..drop);
    }
    entries.reverse();

    let total_input_tokens = totals
        .and_then(|t| t.get("inputTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_output_tokens = totals
        .and_then(|t| t.get("outputTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_cache_creation_tokens = totals
        .and_then(|t| t.get("cacheCreationTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_cache_read_tokens = totals
        .and_then(|t| t.get("cacheReadTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_tokens = totals
        .and_then(|t| t.get("totalTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_cost_usd = totals
        .and_then(|t| t.get("totalCost").and_then(|x| x.as_f64()))
        .unwrap_or(0.0);

    Ok(CcusageDailyResponse {
        entries,
        total_input_tokens,
        total_output_tokens,
        total_cache_creation_tokens,
        total_cache_read_tokens,
        total_tokens,
        total_cost_usd,
    })
}

pub(crate) fn parse_session_json(
    raw: &str,
    limit: u32,
) -> Result<Vec<CcusageSessionEntry>, String> {
    let blob = extract_json_blob(raw)
        .ok_or_else(|| "ccusage session: no JSON object found in stdout".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(blob).map_err(|e| format!("ccusage session: invalid JSON: {}", e))?;
    let arr = v
        .get("sessions")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "ccusage session: missing `sessions` array".to_string())?;
    let mut rows: Vec<CcusageSessionEntry> = arr
        .iter()
        .map(|item| {
            let models_used: Vec<String> = item
                .get("modelsUsed")
                .and_then(|x| x.as_array())
                .map(|xs| {
                    xs.iter()
                        .filter_map(|m| m.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let source = classify_source(&models_used);
            CcusageSessionEntry {
                session_id: item
                    .get("sessionId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_activity: item
                    .get("lastActivity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                project: item
                    .get("projectPath")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_tokens: item
                    .get("inputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                output_tokens: item
                    .get("outputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cache_creation_tokens: item
                    .get("cacheCreationTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cache_read_tokens: item
                    .get("cacheReadTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                total_tokens: item
                    .get("totalTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                estimated_cost_usd: item
                    .get("totalCost")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0),
                models_used,
                source,
            }
        })
        .collect();

    // Sort by last_activity DESC, then by total tokens DESC as a tiebreaker.
    rows.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| b.total_tokens.cmp(&a.total_tokens))
    });
    if limit > 0 && rows.len() > limit as usize {
        rows.truncate(limit as usize);
    }
    Ok(rows)
}

#[tauri::command]
pub async fn ccusage_daily(days: u32) -> Result<CcusageDailyResponse, String> {
    let (stdout, stderr, ok) = run_ccusage(&["daily", "--json", "--order", "asc"]).await?;
    if !ok && stdout.trim().is_empty() {
        return Err(format!("ccusage daily failed: {}", stderr.trim()));
    }
    parse_daily_json(&stdout, days)
}

#[tauri::command]
pub async fn ccusage_sessions(limit: u32) -> Result<Vec<CcusageSessionEntry>, String> {
    let (stdout, stderr, ok) = run_ccusage(&["session", "--json"]).await?;
    if !ok && stdout.trim().is_empty() {
        return Err(format!("ccusage session failed: {}", stderr.trim()));
    }
    parse_session_json(&stdout, limit)
}

// ── @ccusage/codex (real codex CLI parser, gpt-5.1-codex pricing) ──────────
//
// `ccusage` (the main pkg) only scans ~/.claude/projects. `@ccusage/codex`
// scans ~/.codex/sessions and applies real OpenAI Codex pricing — including
// gpt-5.1-codex / gpt-5 pricing via LiteLLM. We surface it as a parallel
// command so the GUI can merge both data sources cleanly.

async fn run_ccusage_codex(args: &[&str]) -> Result<(String, String, bool), String> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};

    let mut full = vec!["-y", "@ccusage/codex@latest"];
    full.extend_from_slice(args);
    let fut = Command::new("npx").args(&full).output();
    let res = timeout(Duration::from_secs(60), fut)
        .await
        .map_err(|_| format!("npx @ccusage/codex {:?}: timeout after 60s", args))?
        .map_err(|e| format!("npx @ccusage/codex {:?}: spawn failed: {}", args, e))?;
    Ok((
        String::from_utf8_lossy(&res.stdout).to_string(),
        String::from_utf8_lossy(&res.stderr).to_string(),
        res.status.success(),
    ))
}

/// Parse @ccusage/codex `daily --json` output. Field names differ slightly
/// from main ccusage: `cachedInputTokens` / `reasoningOutputTokens` / `costUSD`
/// / `models` (object keyed by model id).
pub(crate) fn parse_codex_daily_json(raw: &str, days: u32) -> Result<CcusageDailyResponse, String> {
    let json = extract_json_blob(raw)
        .ok_or_else(|| "no JSON object in @ccusage/codex stdout".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("decode @ccusage/codex daily: {}", e))?;
    let arr = v
        .get("daily")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "@ccusage/codex daily: no `daily` array".to_string())?;
    let mut entries: Vec<CcusageDailyEntry> = arr
        .iter()
        .map(|e| {
            let date_raw = e
                .get("date")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            // Normalize date to YYYY-MM-DD when possible (chrono parses
            // many locale formats; fallback to raw).
            let date = chrono::NaiveDate::parse_from_str(&date_raw, "%b %d, %Y")
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or(date_raw);
            let models_used: Vec<String> = e
                .get("models")
                .and_then(|m| m.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            CcusageDailyEntry {
                date,
                input_tokens: e.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: e.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                cache_creation_tokens: 0, // codex doesn't expose write/read split
                cache_read_tokens: e
                    .get("cachedInputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                total_tokens: e.get("totalTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                estimated_cost_usd: e.get("costUSD").and_then(|x| x.as_f64()).unwrap_or(0.0),
                models_used,
                source: "codex",
            }
        })
        .collect();
    // Trim to last N days (entries already sorted oldest-first by ccusage).
    if entries.len() > days as usize {
        let cut = entries.len() - days as usize;
        entries.drain(..cut);
    }
    let totals = v.get("totals");
    let total_input_tokens = totals
        .and_then(|t| t.get("inputTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_output_tokens = totals
        .and_then(|t| t.get("outputTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_cache_read_tokens = totals
        .and_then(|t| t.get("cachedInputTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_tokens = totals
        .and_then(|t| t.get("totalTokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    let total_cost_usd = totals
        .and_then(|t| t.get("costUSD").and_then(|x| x.as_f64()))
        .unwrap_or(0.0);
    Ok(CcusageDailyResponse {
        entries,
        total_input_tokens,
        total_output_tokens,
        total_cache_creation_tokens: 0,
        total_cache_read_tokens,
        total_tokens,
        total_cost_usd,
    })
}

pub(crate) fn parse_codex_session_json(
    raw: &str,
    limit: u32,
) -> Result<Vec<CcusageSessionEntry>, String> {
    let json = extract_json_blob(raw)
        .ok_or_else(|| "no JSON object in @ccusage/codex stdout".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("decode @ccusage/codex session: {}", e))?;
    let arr = v
        .get("sessions")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "@ccusage/codex session: no `sessions` array".to_string())?;
    let mut sessions: Vec<CcusageSessionEntry> = arr
        .iter()
        .map(|e| {
            let models_used: Vec<String> = e
                .get("models")
                .and_then(|m| m.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            CcusageSessionEntry {
                session_id: e
                    .get("sessionFile")
                    .or_else(|| e.get("sessionId"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_activity: e
                    .get("lastActivity")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                project: e
                    .get("directory")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Unknown")
                    .to_string(),
                input_tokens: e.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: e.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                cache_creation_tokens: 0,
                cache_read_tokens: e
                    .get("cachedInputTokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                total_tokens: e.get("totalTokens").and_then(|x| x.as_u64()).unwrap_or(0),
                estimated_cost_usd: e.get("costUSD").and_then(|x| x.as_f64()).unwrap_or(0.0),
                models_used,
                source: "codex",
            }
        })
        .collect();
    sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    if sessions.len() > limit as usize {
        sessions.truncate(limit as usize);
    }
    Ok(sessions)
}

#[tauri::command]
pub async fn ccusage_codex_daily(days: u32) -> Result<CcusageDailyResponse, String> {
    let (stdout, stderr, ok) = run_ccusage_codex(&["daily", "--json"]).await?;
    if !ok && stdout.trim().is_empty() {
        return Err(format!("@ccusage/codex daily failed: {}", stderr.trim()));
    }
    parse_codex_daily_json(&stdout, days)
}

#[tauri::command]
pub async fn ccusage_codex_sessions(limit: u32) -> Result<Vec<CcusageSessionEntry>, String> {
    let (stdout, stderr, ok) = run_ccusage_codex(&["session", "--json"]).await?;
    if !ok && stdout.trim().is_empty() {
        return Err(format!("@ccusage/codex session failed: {}", stderr.trim()));
    }
    parse_codex_session_json(&stdout, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_lt_handles_double_digits() {
        assert!(semver_lt("0.9.0", "0.10.0"));
        assert!(!semver_lt("0.10.0", "0.9.0"));
        assert!(semver_lt("0.39.0", "0.40.0"));
        assert!(!semver_lt("0.39.0", "0.39.0"));
        assert!(semver_lt("1.2.3", "1.2.4"));
    }

    #[test]
    fn rtk_gain_summary_parses() {
        // Real shape produced by `rtk gain --format json` on rtk 0.39.0.
        let raw = r#"{
            "summary": {
                "total_commands": 18,
                "total_input": 3887,
                "total_output": 2131,
                "total_saved": 1767,
                "avg_savings_pct": 45.4592230511963,
                "total_time_ms": 683,
                "avg_time_ms": 37
            }
        }"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let summary = v.get("summary").unwrap().clone();
        let parsed: RtkGain = serde_json::from_value(summary).unwrap();
        assert_eq!(parsed.total_commands, 18);
        assert_eq!(parsed.total_saved, 1767);
        assert_eq!(parsed.total_time_ms, 683);
        assert!((parsed.avg_savings_pct - 45.459).abs() < 0.01);
    }

    #[test]
    fn parse_show_text_extracts_per_client_state() {
        let claude = "[ok] Hook: rtk hook claude (native binary command)\n\
                     [ok] settings.json: RTK hook configured\n\
                     [--] Cursor hook: not found\n";
        assert!(parse_show_for_claude_hook(claude));
        assert!(!parse_show_for_cursor_hook(claude));

        let cursor = "[ok] Cursor hook: configured at ~/.cursor/hooks/rtk\n";
        assert!(parse_show_for_cursor_hook(cursor));

        let codex = "[ok] Global AGENTS.md: RTK.md reference\n";
        assert!(parse_show_for_codex(codex));

        let codex_off = "[--] Global AGENTS.md: RTK.md reference\n";
        assert!(!parse_show_for_codex(codex_off));
    }

    #[test]
    fn extract_json_blob_strips_npm_warn_tail() {
        let raw = r#"{
  "daily": [
    { "date": "2026-05-09", "inputTokens": 1, "outputTokens": 2, "totalTokens": 3 }
  ],
  "totals": { "totalTokens": 3, "totalCost": 0.12 }
}
npm warn Unknown user config "electron_mirror".
npm warn Unknown user config "home".
"#;
        let blob = extract_json_blob(raw).expect("must find json blob");
        let v: serde_json::Value = serde_json::from_str(blob).unwrap();
        assert_eq!(v["totals"]["totalTokens"].as_u64(), Some(3));
    }

    #[test]
    fn ccusage_daily_parses_real_shape() {
        // Trimmed copy of `npx ccusage@latest daily --json` output, with the
        // npm-warn tail npx leaks under a customised npm config.
        let raw = r#"{
  "daily": [
    {
      "date": "2026-03-05",
      "inputTokens": 40,
      "outputTokens": 264,
      "cacheCreationTokens": 0,
      "cacheReadTokens": 85312,
      "totalTokens": 85616,
      "totalCost": 0.0008695,
      "modelsUsed": ["glm-5"],
      "modelBreakdowns": []
    },
    {
      "date": "2026-05-09",
      "inputTokens": 1416,
      "outputTokens": 485530,
      "cacheCreationTokens": 9354223,
      "cacheReadTokens": 304052354,
      "totalTokens": 313893523,
      "totalCost": 222.6354,
      "modelsUsed": ["claude-opus-4-7"],
      "modelBreakdowns": []
    }
  ],
  "totals": {
    "inputTokens": 1456,
    "outputTokens": 485794,
    "cacheCreationTokens": 9354223,
    "cacheReadTokens": 304137666,
    "totalTokens": 313979139,
    "totalCost": 222.6362695
  }
}
npm warn Unknown user config "home". This will stop working.
"#;
        let resp = parse_daily_json(raw, 30).expect("parse ok");
        assert_eq!(resp.entries.len(), 2);
        // Entries returned in DESCENDING date order.
        assert_eq!(resp.entries[0].date, "2026-05-09");
        assert_eq!(resp.entries[0].source, "claude");
        assert_eq!(resp.entries[1].date, "2026-03-05");
        assert_eq!(resp.entries[1].source, "codex");
        assert_eq!(resp.total_tokens, 313_979_139);
        assert!((resp.total_cost_usd - 222.6362695).abs() < 1e-6);
    }

    #[test]
    fn ccusage_session_parses_and_sorts_by_recency() {
        let raw = r#"{
  "sessions": [
    {
      "sessionId": "old-codex-session",
      "inputTokens": 10,
      "outputTokens": 20,
      "cacheCreationTokens": 0,
      "cacheReadTokens": 0,
      "totalTokens": 30,
      "totalCost": 0.001,
      "lastActivity": "2026-04-01",
      "modelsUsed": ["glm-5"],
      "modelBreakdowns": [],
      "projectPath": "Unknown Project"
    },
    {
      "sessionId": "fresh-claude-session",
      "inputTokens": 100,
      "outputTokens": 200,
      "cacheCreationTokens": 0,
      "cacheReadTokens": 0,
      "totalTokens": 300,
      "totalCost": 0.5,
      "lastActivity": "2026-05-09",
      "modelsUsed": ["claude-opus-4-7"],
      "modelBreakdowns": [],
      "projectPath": "-Users-x-some-project"
    }
  ],
  "totals": { "totalTokens": 330, "totalCost": 0.501 }
}"#;
        let rows = parse_session_json(raw, 10).expect("parse ok");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, "fresh-claude-session");
        assert_eq!(rows[0].source, "claude");
        assert_eq!(rows[1].source, "codex");
    }

    #[test]
    fn classify_source_handles_mixed_models() {
        assert_eq!(classify_source(&[]), "none");
        assert_eq!(classify_source(&["claude-opus-4-7".into()]), "claude");
        assert_eq!(classify_source(&["glm-5".into()]), "codex");
        assert_eq!(
            classify_source(&["claude-haiku-4-5-20251001".into(), "gpt-5".into()]),
            "mixed"
        );
    }
}
