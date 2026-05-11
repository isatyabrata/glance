//! `chrome` — drive the user's live Chrome via a glance-owned extension.
//!
//! Bridges to a small MV3 extension over native messaging (see
//! `assets/chrome-bridge/` and `backend::chrome_bridge`). Unlike the
//! `chrome-devtools` MCP this does NOT need `--remote-debugging-port` — it
//! reuses the user's actual Chrome profile, cookies, extensions, and open
//! tabs by piggybacking on `chrome.scripting` / `chrome.debugger` from inside
//! Chrome itself.
//!
//! Tool design: ONE tool with an `action` discriminator + a flat input schema.
//! The schema does NOT use `oneOf`/`anyOf`/`allOf` at the top level — Anthropic
//! rejects that. All action-specific fields are optional siblings, validated
//! inside.
//!
//! Available actions:
//! - `list_tabs`            — list every tab Chrome currently has open
//! - `navigate`             — point a tab at a URL (`tab_id`, `url`)
//! - `wait_load`            — wait for `status === "complete"` (`tab_id`)
//! - `evaluate`             — run an expression in the page's MAIN world
//! - `click`                — querySelector + click (sugar over evaluate)
//! - `fill`                 — set an input's value and dispatch input/change
//! - `screenshot`           — capture the visible tab to a PNG on disk
//! - `lighthouse_audit`     — run Google Lighthouse against a tab's URL
//! - `cdp`                  — raw CDP `Method` + params via chrome.debugger

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backend::chrome_bridge;
use crate::mcp::protocol::{CallToolResult, ToolDefinition};

/// Maximum captured `expression` length (100 KB) — larger ones are dropped to
/// avoid memory bloat on the in-process cache.
const MAX_CAPTURED_EXPRESSION: usize = 100 * 1024;

/// In-memory record of the last successful `evaluate` per tab, used by the
/// "save last evaluate as adapter" auto-capture flow. Empties on glance-mcp
/// restart — that's intentional.
#[derive(Debug, Clone)]
pub struct EvaluateRecord {
    pub expression: String,
    pub await_promise: bool,
    pub ts: u64,
    pub tab_url: String,
}

static LAST_EVALUATE: Lazy<Mutex<HashMap<i64, EvaluateRecord>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Public accessor: returns the last successful evaluate record for `tab_id`,
/// if any. Used by the GUI's "save as adapter" Tauri command.
pub fn get_last_evaluate(tab_id: i64) -> Option<EvaluateRecord> {
    LAST_EVALUATE.lock().ok().and_then(|g| g.get(&tab_id).cloned())
}

fn record_last_evaluate(tab_id: i64, expression: &str, await_promise: bool, tab_url: String) {
    if expression.len() > MAX_CAPTURED_EXPRESSION {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut g) = LAST_EVALUATE.lock() {
        g.insert(
            tab_id,
            EvaluateRecord {
                expression: expression.to_string(),
                await_promise,
                ts,
                tab_url,
            },
        );
    }
}

async fn lookup_tab_url(tab_id: i64) -> Option<String> {
    let v = chrome_bridge::call("tabs.list", json!({})).await.ok()?;
    let arr = v.as_array().cloned().unwrap_or_default();
    for t in arr {
        if t.get("id").and_then(|x| x.as_i64()) == Some(tab_id) {
            return t.get("url").and_then(|x| x.as_str()).map(|s| s.to_string());
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    tab_id: Option<i64>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    expression: Option<String>,
    #[serde(default)]
    await_promise: Option<bool>,
    /// `evaluate` execution world. "main" (default) runs via chrome.scripting;
    /// "cdp" runs via chrome.debugger Runtime.evaluate, which bypasses page CSP
    /// (X.com etc. that block `unsafe-eval`).
    #[serde(default)]
    world: Option<String>,
    /// `evaluate world=cdp` only: skip Glance's `(async () => {...})()`
    /// IIFE auto-wrap. Use when you want full control over the raw expression
    /// passed to CDP Runtime.evaluate (e.g. multi-statement scripts where you
    /// own the return logic).
    #[serde(default)]
    raw: Option<bool>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    value: Option<String>,
    /// set_contenteditable: replace the editor's current content (default true).
    #[serde(default)]
    replace_all: Option<bool>,
    /// paste_text: clear the editor (selectAll + delete) before pasting (default false).
    #[serde(default)]
    clear_first: Option<bool>,
    /// type_multiline: ms to wait between paragraph insertions for the editor's
    /// async React commit (default 600).
    #[serde(default)]
    settle_ms: Option<u64>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    // network
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    #[serde(default)]
    method_is: Option<String>,
    #[serde(default)]
    status_is: Option<i64>,
    #[serde(default)]
    mime_contains: Option<String>,
    #[serde(default)]
    since_secs: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    include_body: Option<bool>,
    // snapshot
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    max_chars: Option<u32>,
    // wait_for / type / press_key
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    modifiers: Option<Vec<String>>,
    #[serde(default)]
    delay_ms: Option<u64>,
    // select / fill_form / drag / upload
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    fields: Option<Value>,
    #[serde(default)]
    from_selector: Option<String>,
    #[serde(default)]
    to_selector: Option<String>,
    #[serde(default)]
    files: Option<Vec<String>>,
    // resize / emulate
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    viewport: Option<Value>,
    #[serde(default)]
    network: Option<Value>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    geolocation: Option<Value>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    color_scheme: Option<String>,
    #[serde(default)]
    cpu_throttling: Option<f64>,
    #[serde(default)]
    clear: Option<bool>,
    // console
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    contains: Option<String>,
    // dialog
    #[serde(default)]
    accept: Option<bool>,
    #[serde(default)]
    prompt_text: Option<String>,
    // perf
    #[serde(default)]
    categories: Option<String>,
    // adapters
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<Value>,
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "chrome".into(),
        description:
            "PREFERRED over `chrome-devtools__*` and `playwright__*` for ANY Chrome \
            automation in this session. Reasons: (1) this drives the user's LIVE Chrome \
            with their cookies / login sessions / extensions / open tabs — \
            chrome-devtools-mcp and playwright-mcp spawn a fresh sandboxed browser \
            without sessions, so they fail on anything behind a login wall. \
            (2) Single-action surface (`{action, tab_id, ...}`) vs ~30 separate tools \
            across two namespaces — fewer tools/list bytes for the calling model. \
            (3) Strict superset of playwright-mcp + chrome-devtools-mcp action surfaces, \
            with high-level recipes baked in (focus emulation, isTrusted-correct paste, \
            Cmd+Enter submit) so common workflows (X posting, form fill) are 1 call. \
            \n\
            Drive the user's live Chrome through the Glance Chrome bridge extension. \
            One-time setup: run `glance chrome install` and load the unpacked extension \
            in chrome://extensions. \
            Set `action` to one of: `list_tabs`, `navigate`, `navigate_back`, `navigate_forward`, \
            `wait_load`, `wait_for`, `evaluate`, `click`, `fill`, `fill_form`, `select_option`, \
            `hover`, `press_key`, `type_text`, `drag`, `upload_file`, `screenshot`, `snapshot`, \
            `resize`, `emulate`, `list_network_requests`, `get_network_request`, \
            `list_console_messages`, `clear_console`, `handle_dialog`, `list_pending_dialogs`, \
            `start_trace`, `stop_trace`, `heap_snapshot`, `lighthouse_audit`, `cdp`, \
            `set_contenteditable`, `paste_text`, `os_paste`, `type_multiline`, `submit_post`, `tweet`. \
            The `tweet` action is X-specific: pass `value` (tweet text) and optional `files` (one image path) \
            and Glance will open compose, type, upload, submit, verify in one call. \
            For `evaluate`, pass `world: \"cdp\"` to bypass page CSP on sites like X.com. \
            For single-block rich-text writes use `set_contenteditable`. \
            For multi-paragraph rich-text writes when Chrome HAS OS focus, `os_paste` \
            is fastest (pbcopy + CDP commands:[\"paste\"]). When Chrome is in the \
            background (Glance must run silently without stealing focus), use \
            `type_multiline` — it inserts each paragraph via CDP Input.insertText with \
            Enter keys between blocks, all isTrusted=true and routed to the focused \
            element regardless of window focus. \
            Most actions take `tab_id`."
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list_tabs", "navigate", "navigate_back", "navigate_forward",
                        "wait_load", "wait_for",
                        "evaluate", "click", "fill", "fill_form", "select_option",
                        "hover", "press_key", "type_text", "drag", "upload_file",
                        "screenshot", "snapshot",
                        "resize", "emulate",
                        "list_network_requests", "get_network_request",
                        "list_console_messages", "clear_console",
                        "handle_dialog", "list_pending_dialogs",
                        "start_trace", "stop_trace", "heap_snapshot",
                        "lighthouse_audit",
                        "cdp",
                        "set_contenteditable",
                        "paste_text",
                        "os_paste",
                        "type_multiline",
                        "submit_post",
                        "tweet",
                        "list_adapters", "run_adapter"
                    ],
                    "description": "Which sub-action to run."
                },
                "tab_id": { "type": "integer", "description": "Chrome tab id (from list_tabs)." },
                "url": { "type": "string", "description": "Target URL for `navigate`." },
                "expression": {
                    "type": "string",
                    "description": "JS expression for `evaluate`. Runs in MAIN world. Last expression's value is returned."
                },
                "await_promise": {
                    "type": "boolean",
                    "description": "If true, await the expression when it returns a Promise."
                },
                "world": {
                    "type": "string",
                    "enum": ["main", "cdp"],
                    "description": "evaluate execution world. 'main' (default) is fast but subject to page CSP. 'cdp' uses the debugger's Runtime.evaluate and bypasses CSP — use it on X.com and other strict-CSP sites."
                },
                "raw": {
                    "type": "boolean",
                    "description": "evaluate world=cdp only: skip Glance's automatic IIFE wrapping. Use when the expression contains top-level statements you don't want wrapped, or when you want raw control of the script body (the IIFE wrap is skipped automatically when the expression already starts with `(` or `async`, so this flag is mostly for multi-statement scripts that return via console.log)."
                },
                "selector": { "type": "string", "description": "CSS selector for `click` / `fill` / `set_contenteditable` / `paste_text`." },
                "value": { "type": "string", "description": "Value to set / paste in `fill` / `set_contenteditable` / `paste_text`." },
                "replace_all": { "type": "boolean", "description": "set_contenteditable: replace current content (default true)." },
                "clear_first": { "type": "boolean", "description": "paste_text: selectAll + delete the editor before pasting (default false). Use for re-filling an editor that already has text." },
                "settle_ms": { "type": "integer", "description": "type_multiline: ms to wait between paragraph insertions for the editor's React commit (default 600)." },
                "format": {
                    "type": "string",
                    "enum": ["png", "jpeg"],
                    "description": "Screenshot format (default png)."
                },
                "save_path": {
                    "type": "string",
                    "description": "Where to write the screenshot. Defaults to ~/.glance/cache/chrome-<ts>.png."
                },
                "timeout_ms": { "type": "integer", "description": "Override default timeout (wait_load)." },
                "method": { "type": "string", "description": "CDP method name for `cdp` (e.g. \"Page.captureScreenshot\")." },
                "params": { "type": "object", "description": "CDP params object for `cdp`." },
                "request_id": { "type": "string", "description": "CDP request id (for get_network_request). Get from list_network_requests output." },
                "url_contains": { "type": "string", "description": "list_network_requests filter: substring of URL." },
                "method_is": { "type": "string", "description": "list_network_requests filter: exact HTTP method." },
                "status_is": { "type": "integer", "description": "list_network_requests filter: exact HTTP status." },
                "mime_contains": { "type": "string", "description": "list_network_requests filter: substring of Content-Type." },
                "since_secs": { "type": "integer", "description": "list_network_requests filter: only requests started in the last N seconds." },
                "limit": { "type": "integer", "description": "list_network_requests max rows (default 100, hard cap 500)." },
                "include_body": { "type": "boolean", "description": "get_network_request: fetch the response body too (default true)." },
                "mode": {
                    "type": "string",
                    "enum": ["text", "html", "a11y"],
                    "description": "snapshot mode. text=clean innerText, html=stripped outerHTML, a11y=accessibility tree. Default text."
                },
                "max_chars": { "type": "integer", "description": "snapshot character cap (default 8000, hard cap 200000)." },
                "text": { "type": "string", "description": "wait_for: text to wait for. type_text: characters to type." },
                "key": { "type": "string", "description": "press_key: key name (Enter, Tab, Escape, ArrowDown, a-z, etc)." },
                "modifiers": { "type": "array", "items": { "type": "string" }, "description": "press_key modifiers: any of Alt, Ctrl, Meta, Shift." },
                "delay_ms": { "type": "integer", "description": "type_text inter-character delay (default 0)." },
                "label": { "type": "string", "description": "select_option: option label / text content (alternative to value)." },
                "fields": { "type": "array", "description": "fill_form: array of {selector, value} objects." },
                "from_selector": { "type": "string", "description": "drag: source selector." },
                "to_selector": { "type": "string", "description": "drag: target selector." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "upload_file: absolute paths to upload." },
                "width": { "type": "integer", "description": "resize / emulate: viewport width." },
                "height": { "type": "integer", "description": "resize / emulate: viewport height." },
                "viewport": { "type": "object", "description": "emulate: { width, height, deviceScaleFactor, mobile }." },
                "network": { "description": "emulate: 'offline' | 'slow-3g' | 'fast-3g' | 'slow-4g' | 'fast-4g' | object." },
                "user_agent": { "type": "string", "description": "emulate: override navigator.userAgent." },
                "geolocation": { "type": "object", "description": "emulate: { latitude, longitude, accuracy }." },
                "timezone": { "type": "string", "description": "emulate: IANA timezone (e.g. 'Asia/Shanghai')." },
                "color_scheme": { "type": "string", "enum": ["dark", "light"], "description": "emulate: prefers-color-scheme." },
                "cpu_throttling": { "type": "number", "description": "emulate: CPU slowdown factor (1 = none, 4 = 4x slower)." },
                "clear": { "type": "boolean", "description": "emulate: clear all overrides." },
                "level": { "type": "string", "description": "list_console_messages filter: log/info/warn/error/debug." },
                "contains": { "type": "string", "description": "list_console_messages filter: substring." },
                "accept": { "type": "boolean", "description": "handle_dialog: accept (true) or dismiss (false)." },
                "prompt_text": { "type": "string", "description": "handle_dialog: text to type into a prompt() dialog." },
                "categories": {
                    "type": "string",
                    "description": "start_trace: comma-separated CDP trace categories. lighthouse_audit: comma-separated subset of `performance,accessibility,best-practices,seo,pwa` (default `performance,accessibility,best-practices,seo`)."
                },
                "name": { "type": "string", "description": "run_adapter: adapter name (see list_adapters)." },
                "args": { "type": "object", "description": "run_adapter: per-adapter argument map (e.g. {date:'2026-05-10'})." }
            },
            "required": ["action"]
        }),
    }
}

pub async fn call(args: Value) -> Result<CallToolResult> {
    let a: Args = match serde_json::from_value(args) {
        Ok(v) => v,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] bad args: {}", e))),
    };

    match a.action.as_str() {
        "list_tabs" => list_tabs().await,
        "navigate" => navigate(a).await,
        "wait_load" => wait_load(a).await,
        "evaluate" => evaluate(a).await,
        "click" => click(a).await,
        "fill" => fill(a).await,
        "screenshot" => screenshot(a).await,
        "snapshot" => snapshot(a).await,
        "wait_for" => wait_for(a).await,
        "navigate_back" => navigate_back(a).await,
        "navigate_forward" => navigate_forward(a).await,
        "press_key" => press_key(a).await,
        "type_text" => type_text(a).await,
        "hover" => hover(a).await,
        "select_option" => select_option(a).await,
        "fill_form" => fill_form(a).await,
        "drag" => drag(a).await,
        "upload_file" => upload_file(a).await,
        "resize" => resize(a).await,
        "emulate" => emulate(a).await,
        "list_console_messages" => list_console_messages(a).await,
        "clear_console" => clear_console(a).await,
        "handle_dialog" => handle_dialog(a).await,
        "list_pending_dialogs" => list_pending_dialogs(a).await,
        "start_trace" => start_trace(a).await,
        "stop_trace" => stop_trace(a).await,
        "heap_snapshot" => heap_snapshot(a).await,
        "lighthouse_audit" => lighthouse_audit(a).await,
        "list_network_requests" => list_network_requests(a).await,
        "get_network_request" => get_network_request(a).await,
        "list_adapters" => list_adapters(a).await,
        "run_adapter" => run_adapter(a).await,
        "cdp" => cdp(a).await,
        "set_contenteditable" => set_contenteditable(a).await,
        "paste_text" => paste_text(a).await,
        "os_paste" => os_paste(a).await,
        "type_multiline" => type_multiline(a).await,
        "submit_post" => submit_post(a).await,
        "tweet" => tweet(a).await,
        other => Ok(CallToolResult::error(format!(
            "[chrome] unknown action: {}",
            other
        ))),
    }
}

async fn list_tabs() -> Result<CallToolResult> {
    let v = chrome_bridge::call("tabs.list", json!({})).await;
    match v {
        Ok(val) => {
            let arr = val.as_array().cloned().unwrap_or_default();
            let mut out = String::from("# Chrome tabs\n\n");
            for t in &arr {
                let id = t.get("id").and_then(|x| x.as_i64()).unwrap_or(-1);
                let title = t.get("title").and_then(|x| x.as_str()).unwrap_or("");
                let url = t.get("url").and_then(|x| x.as_str()).unwrap_or("");
                let active = t.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
                let mark = if active { "★" } else { " " };
                out.push_str(&format!("{} `{}` — {}\n   {}\n", mark, id, title, url));
            }
            if arr.is_empty() {
                out.push_str("(no tabs)\n");
            }
            Ok(CallToolResult::text(out))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] list_tabs: {}", e))),
    }
}

async fn navigate(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] navigate needs tab_id")),
    };
    let url = match a.url {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] navigate needs url")),
    };
    let r = chrome_bridge::call("tabs.navigate", json!({ "tabId": tab_id, "url": url })).await;
    match r {
        Ok(_) => Ok(CallToolResult::text(format!(
            "[chrome] tab {} navigating to {}",
            tab_id, url
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] navigate: {}", e))),
    }
}

async fn wait_load(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] wait_load needs tab_id")),
    };
    let timeout = a.timeout_ms.unwrap_or(15_000);
    let r = chrome_bridge::call(
        "tabs.wait_load",
        json!({ "tabId": tab_id, "timeoutMs": timeout }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] wait_load: {}",
            v
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] wait_load: {}", e))),
    }
}

async fn evaluate(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] evaluate needs tab_id")),
    };
    let expression = match a.expression {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] evaluate needs expression")),
    };
    let await_promise = a.await_promise.unwrap_or(false);
    let world = a
        .world
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "main".into());
    if world != "main" && world != "cdp" {
        return Ok(CallToolResult::error(format!(
            "[chrome] evaluate: unknown world '{}' (use 'main' or 'cdp')",
            world
        )));
    }
    let mut params = json!({
        "tabId": tab_id,
        "expression": expression,
        "awaitPromise": await_promise,
        "world": world,
    });
    if a.raw.unwrap_or(false) {
        params["raw"] = json!(true);
    }
    let r = chrome_bridge::call("tabs.evaluate", params).await;
    match r {
        Ok(v) => {
            // Only "successful" evaluates that returned a non-error, non-null
            // value qualify for save-as-adapter capture. Throws/no-return are
            // most likely scratch debugging that we don't want to persist.
            let captured_useful = !v.is_null();
            if captured_useful {
                let url = lookup_tab_url(tab_id).await.unwrap_or_default();
                record_last_evaluate(tab_id, &expression, await_promise, url);
            }
            let s = if v.is_string() {
                v.as_str().unwrap().to_string()
            } else {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            };
            Ok(CallToolResult::text(s))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] evaluate: {}", e))),
    }
}

async fn click(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] click needs tab_id")),
    };
    let sel = match a.selector {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] click needs selector")),
    };
    // v0.49: native ISOLATED-world handler — no eval, immune to strict page
    // CSP (X.com etc). Dispatches real MouseEvents at element center.
    let r = chrome_bridge::call(
        "tabs.click_native",
        json!({ "tabId": tab_id, "selector": sel }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] click: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] click: {}", e))),
    }
}

async fn fill(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] fill needs tab_id")),
    };
    let sel = match a.selector {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] fill needs selector")),
    };
    let value = a.value.unwrap_or_default();
    // v0.49: native ISOLATED-world handler — no eval, immune to page CSP.
    let r = chrome_bridge::call(
        "tabs.fill_native",
        json!({ "tabId": tab_id, "selector": sel, "value": value }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] fill: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] fill: {}", e))),
    }
}

async fn set_contenteditable(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] set_contenteditable needs tab_id")),
    };
    let selector = match a.selector {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] set_contenteditable needs selector")),
    };
    let value = a.value.unwrap_or_default();
    let replace_all = a.replace_all.unwrap_or(true);
    let r = chrome_bridge::call(
        "tabs.set_contenteditable",
        json!({
            "tabId": tab_id,
            "selector": selector,
            "value": value,
            "replaceAll": replace_all,
        }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] set_contenteditable: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] set_contenteditable: {}", e))),
    }
}

async fn paste_text(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] paste_text needs tab_id")),
    };
    let selector = match a.selector {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] paste_text needs selector")),
    };
    let value = a.value.unwrap_or_default();
    let clear_first = a.clear_first.unwrap_or(false);
    let r = chrome_bridge::call(
        "tabs.paste_text",
        json!({
            "tabId": tab_id,
            "selector": selector,
            "value": value,
            "clearFirst": clear_first,
        }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] paste_text: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] paste_text: {}", e))),
    }
}

// OS-level paste: write text to the system clipboard via pbcopy/xclip/clip,
// then have the bridge focus the selector and trigger a real CDP paste. This
// survives Draft.js / Lexical isTrusted checks that block synthetic
// ClipboardEvents.
async fn os_paste(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] os_paste needs tab_id")),
    };
    let selector = a.selector;
    let value = a.value.unwrap_or_default();

    // 1. Write to system clipboard.
    if let Err(e) = write_clipboard(&value).await {
        return Ok(CallToolResult::error(format!(
            "[chrome] os_paste: clipboard write failed: {}",
            e
        )));
    }

    // 2. Focus the selector (if given) + dispatch real OS-level paste via CDP.
    let mut params = json!({ "tabId": tab_id });
    if let Some(s) = selector {
        params["selector"] = json!(s);
    }
    let r = chrome_bridge::call("tabs.paste_keyboard", params).await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] os_paste: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] os_paste: {}", e))),
    }
}

// Insert multi-paragraph text into focused element without OS focus and
// without touching the system clipboard. Each paragraph goes in as a single
// CDP Input.insertText (atomic — Draft.js sees one beforeinput) with Enter
// keys between blocks and a settle wait per block for React to commit.
async fn type_multiline(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] type_multiline needs tab_id")),
    };
    let value = a.value.unwrap_or_default();
    if value.is_empty() {
        return Ok(CallToolResult::error("[chrome] type_multiline needs value"));
    }
    let mut params = json!({ "tabId": tab_id, "value": value });
    if let Some(s) = a.selector {
        params["selector"] = json!(s);
    }
    if let Some(ms) = a.settle_ms {
        params["settleMs"] = json!(ms);
    }
    let r = chrome_bridge::call("tabs.type_multiline", params).await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] type_multiline: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] type_multiline: {}", e))),
    }
}

// Submit a rich-text editor via OS-level Cmd+Enter (trusted keyboard event).
// Replaces synthetic clicks on submit buttons that would be flagged as
// automation. Works with X's compose modal, Bluesky, most editors that
// honor the standard "Cmd+Enter to send" shortcut.
async fn submit_post(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] submit_post needs tab_id")),
    };
    let mut params = json!({ "tabId": tab_id });
    if let Some(s) = a.selector {
        params["selector"] = json!(s);
    }
    let r = chrome_bridge::call("tabs.submit_post", params).await;
    match r {
        Ok(v) => Ok(CallToolResult::text(format!(
            "[chrome] submit_post: {}",
            serde_json::to_string(&v).unwrap_or_default()
        ))),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] submit_post: {}", e))),
    }
}

// X-specific one-shot tweet poster. Orchestrates the full silent-background
// flow: focus emulation -> open compose -> type -> upload image -> review
// pause -> Cmd+Enter submit -> verify URL. All Glance-internal primitives,
// so this is the canonical "send tweet" recipe.
async fn tweet(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] tweet needs tab_id")),
    };
    let text = a.value.clone().unwrap_or_default();
    if text.is_empty() {
        return Ok(CallToolResult::error("[chrome] tweet needs value (tweet text)"));
    }
    let image = a.files.as_ref().and_then(|f| f.first().cloned());
    let editor_sel = "[role=\"dialog\"] [data-testid=\"tweetTextarea_0\"]";
    let file_sel = "[role=\"dialog\"] input[data-testid=\"fileInput\"]";

    // 1. Force docHasFocus=true so X mounts the compose modal content even
    //    when Chrome is in the background (lazy-render gate).
    if let Err(e) = chrome_bridge::call(
        "cdp.send",
        json!({
            "tabId": tab_id,
            "method": "Emulation.setFocusEmulationEnabled",
            "params": { "enabled": true }
        }),
    )
    .await
    {
        return Ok(CallToolResult::error(format!("[chrome] tweet focus emu: {}", e)));
    }

    // 2. Open compose: click SideNav button via DOM.click() (page-side React
    //    handler), then poll for the dialog editor to mount (height > 30).
    let open_expr = r#"(async () => {
        const btn = document.querySelector('[data-testid="SideNav_NewTweet_Button"]');
        if (!btn) throw new Error('SideNav Post button not found');
        btn.click();
        for (let i = 0; i < 30; i++) {
            await new Promise(r => setTimeout(r, 200));
            const editor = document.querySelector('[role="dialog"] [data-testid="tweetTextarea_0"]');
            if (editor && editor.getBoundingClientRect().height > 30) {
                return { ok: true, elapsedMs: (i + 1) * 200 };
            }
        }
        return { ok: false, error: 'modal did not mount within 6s' };
    })()"#;
    let modal_check = chrome_bridge::call(
        "tabs.evaluate",
        json!({
            "tabId": tab_id,
            "expression": open_expr,
            "awaitPromise": true,
            "world": "cdp"
        }),
    )
    .await;
    match modal_check {
        Ok(v) => {
            if v.get("ok") != Some(&json!(true)) {
                return Ok(CallToolResult::error(format!(
                    "[chrome] tweet open_modal failed: {}",
                    serde_json::to_string(&v).unwrap_or_default()
                )));
            }
        }
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] tweet open_modal: {}", e))),
    }

    // 3. Type text into the modal editor (focus-free, multi-paragraph safe).
    if let Err(e) = chrome_bridge::call(
        "tabs.type_multiline",
        json!({
            "tabId": tab_id,
            "selector": editor_sel,
            "value": text,
            "settleMs": 600,
        }),
    )
    .await
    {
        return Ok(CallToolResult::error(format!("[chrome] tweet type: {}", e)));
    }

    // 4. Optional image upload.
    if let Some(img_path) = image.as_ref() {
        if let Err(e) = chrome_bridge::call(
            "tabs.upload_file",
            json!({
                "tabId": tab_id,
                "selector": file_sel,
                "files": [img_path],
            }),
        )
        .await
        {
            return Ok(CallToolResult::error(format!("[chrome] tweet upload: {}", e)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
    }

    // 5. 4s review pause (mimics a user re-reading before sending).
    tokio::time::sleep(std::time::Duration::from_millis(4000)).await;

    // 6. Submit via OS-trusted Cmd+Enter.
    if let Err(e) = chrome_bridge::call(
        "tabs.submit_post",
        json!({
            "tabId": tab_id,
            "selector": editor_sel,
        }),
    )
    .await
    {
        return Ok(CallToolResult::error(format!("[chrome] tweet submit: {}", e)));
    }

    // 7. Let X commit.
    tokio::time::sleep(std::time::Duration::from_millis(5500)).await;

    // 8. Capture posted tweet URL by checking newest /status/ link on home.
    let verify_expr = r#"(async () => {
        const links = Array.from(document.querySelectorAll('a[href*="/status/"]'))
            .map(a => a.href)
            .filter(h => /^https?:\/\/x\.com\/[^/]+\/status\/\d+$/.test(h));
        return { url: location.href, latest: links[0] || null };
    })()"#;
    let verify = chrome_bridge::call(
        "tabs.evaluate",
        json!({
            "tabId": tab_id,
            "expression": verify_expr,
            "awaitPromise": true,
            "world": "cdp"
        }),
    )
    .await;
    let latest = match verify {
        Ok(v) => v.get("latest").and_then(|x| x.as_str()).map(|s| s.to_string()),
        Err(_) => None,
    };

    Ok(CallToolResult::text(format!(
        "[chrome] tweet ✅ {} ({} chars{})",
        latest.unwrap_or_else(|| "(URL not captured — check profile)".into()),
        text.chars().count(),
        if image.is_some() { ", +image" } else { "" }
    )))
}

async fn write_clipboard(text: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let bin = if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(target_os = "windows") {
        "clip"
    } else {
        // Linux: try xclip first, then xsel; fall back is left to the user.
        if std::process::Command::new("xclip").arg("-version").output().is_ok() {
            "xclip"
        } else {
            "xsel"
        }
    };

    let mut cmd = Command::new(bin);
    if bin == "xclip" {
        cmd.arg("-selection").arg("clipboard");
    } else if bin == "xsel" {
        cmd.arg("--clipboard").arg("--input");
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {}", bin, e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("write to {}: {}", bin, e))?;
        stdin.shutdown().await.ok();
    }
    let status = child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("wait {}: {}", bin, e))?;
    if !status.success() {
        return Err(anyhow::anyhow!("{} exited with {}", bin, status));
    }
    Ok(())
}

async fn screenshot(a: Args) -> Result<CallToolResult> {
    let format = a
        .format
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "png".into());
    let format = if format == "jpeg" || format == "jpg" {
        "jpeg"
    } else {
        "png"
    };

    let r = chrome_bridge::call(
        "tabs.screenshot",
        json!({ "tabId": a.tab_id, "format": format }),
    )
    .await;
    let v = match r {
        Ok(v) => v,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] screenshot: {}", e))),
    };
    let b64 = match v.get("base64").and_then(|x| x.as_str()) {
        Some(s) => s,
        None => return Ok(CallToolResult::error("[chrome] screenshot: no base64 in response")),
    };
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let bytes = match STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] screenshot decode: {}", e))),
    };

    let path = match a.save_path {
        Some(p) => PathBuf::from(p),
        None => default_screenshot_path(format),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, &bytes) {
        return Ok(CallToolResult::error(format!(
            "[chrome] screenshot write: {}",
            e
        )));
    }
    Ok(CallToolResult::text(format!(
        "[chrome] screenshot saved: {} ({} bytes, {})",
        path.display(),
        bytes.len(),
        format
    )))
}

fn default_screenshot_path(format: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".glance/cache").join(format!(
        "chrome-{}.{}",
        ts,
        if format == "jpeg" { "jpg" } else { "png" }
    ))
}

async fn snapshot(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] snapshot needs tab_id")),
    };
    let mode = a.mode.as_deref().unwrap_or("text");
    let max = a.max_chars.unwrap_or(8000);
    let r = chrome_bridge::call(
        "tabs.snapshot",
        json!({ "tabId": tab_id, "mode": mode, "maxChars": max }),
    )
    .await;
    match r {
        Ok(v) => {
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let truncated = v.get("truncated").and_then(|x| x.as_bool()).unwrap_or(false);
            let total = v.get("totalChars").and_then(|x| x.as_u64()).unwrap_or(0);
            let mode_out = v.get("mode").and_then(|x| x.as_str()).unwrap_or(mode);
            let header = format!(
                "<!-- snapshot mode={} chars={} truncated={} total={} -->\n",
                mode_out, content.len(), truncated, total
            );
            Ok(CallToolResult::text(format!("{}{}", header, content)))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] snapshot: {}", e))),
    }
}

async fn list_network_requests(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] list_network_requests needs tab_id")),
    };
    let params = json!({
        "tabId": tab_id,
        "urlContains":   a.url_contains,
        "methodIs":      a.method_is,
        "statusIs":      a.status_is,
        "mimeContains":  a.mime_contains,
        "sinceSecs":     a.since_secs,
        "limit":         a.limit.unwrap_or(100),
        "includePending": false,
    });
    let r = chrome_bridge::call("network.list", params).await;
    match r {
        Ok(v) => {
            let count = v.get("count").and_then(|x| x.as_u64()).unwrap_or(0);
            let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            let arr = v.get("requests").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let mut out = format!(
                "# Network requests (showing {}/{} buffered, tab {})\n\n",
                count, total, tab_id
            );
            out.push_str("| # | method | status | mime | bytes | url | request_id |\n");
            out.push_str("|---|---|---|---|---|---|---|\n");
            for (i, req) in arr.iter().enumerate() {
                let method = req.get("method").and_then(|x| x.as_str()).unwrap_or("");
                let status = req.get("status").and_then(|x| x.as_i64()).map(|n| n.to_string()).unwrap_or_else(|| "—".into());
                let mime = req.get("mime").and_then(|x| x.as_str()).unwrap_or("");
                let bytes = req.get("bytes").and_then(|x| x.as_u64()).map(human_bytes).unwrap_or_else(|| "—".into());
                let url = req.get("url").and_then(|x| x.as_str()).unwrap_or("");
                let url_short = if url.len() > 80 { format!("{}…", &url[..80]) } else { url.to_string() };
                let rid = req.get("requestId").and_then(|x| x.as_str()).unwrap_or("");
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | `{}` | `{}` |\n",
                    i + 1, method, status, mime, bytes, url_short, rid
                ));
            }
            if arr.is_empty() {
                out.push_str("\n_(no matches — note: only captures while a tab has been used since this session started; reload the page to repopulate.)_\n");
            } else {
                out.push_str("\nUse `get_network_request` with a `request_id` to fetch full headers + body.\n");
            }
            Ok(CallToolResult::text(out))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] list_network_requests: {}", e))),
    }
}

async fn get_network_request(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] get_network_request needs tab_id")),
    };
    let request_id = match a.request_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] get_network_request needs request_id")),
    };
    let r = chrome_bridge::call(
        "network.get",
        json!({
            "tabId": tab_id,
            "requestId": request_id,
            "includeBody": a.include_body.unwrap_or(true)
        }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        )),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] get_network_request: {}", e))),
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 { format!("{} B", n) }
    else if n < 1024 * 1024 { format!("{:.1} KB", n as f64 / 1024.0) }
    else { format!("{:.1} MB", n as f64 / 1024.0 / 1024.0) }
}

async fn cdp(a: Args) -> Result<CallToolResult> {
    let tab_id = match a.tab_id {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] cdp needs tab_id")),
    };
    let method = match a.method {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] cdp needs method")),
    };
    let r = chrome_bridge::call(
        "cdp.send",
        json!({ "tabId": tab_id, "method": method, "params": a.params.unwrap_or_else(|| json!({})) }),
    )
    .await;
    match r {
        Ok(v) => Ok(CallToolResult::text(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        )),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] cdp: {}", e))),
    }
}

// ----- v0.41 parity helpers -----
//
// Each helper bridges to one extension method. We surface every option as a
// flat field on Args (no nested oneOf), validate, and forward.

fn need_tab(a: &Args, action: &str) -> Result<i64> {
    a.tab_id.ok_or_else(|| anyhow::anyhow!("[chrome] {} needs tab_id", action))
}

async fn forward(method: &str, params: Value) -> Result<CallToolResult> {
    match chrome_bridge::call(method, params).await {
        Ok(v) => Ok(CallToolResult::text(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        )),
        Err(e) => Ok(CallToolResult::error(format!("[chrome] {}: {}", method, e))),
    }
}

async fn wait_for(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "wait_for")?;
    if a.selector.is_none() && a.text.is_none() {
        return Ok(CallToolResult::error("[chrome] wait_for needs selector or text"));
    }
    forward(
        "tabs.wait_for",
        json!({
            "tabId": tab, "selector": a.selector, "text": a.text,
            "timeoutMs": a.timeout_ms.unwrap_or(10_000),
        }),
    )
    .await
}

async fn navigate_back(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "navigate_back")?;
    forward("tabs.navigate_back", json!({ "tabId": tab })).await
}
async fn navigate_forward(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "navigate_forward")?;
    forward("tabs.navigate_forward", json!({ "tabId": tab })).await
}

async fn press_key(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "press_key")?;
    let key = a.key.ok_or_else(|| anyhow::anyhow!("[chrome] press_key needs key"))?;
    forward(
        "tabs.press_key",
        json!({ "tabId": tab, "key": key, "modifiers": a.modifiers.unwrap_or_default() }),
    )
    .await
}

async fn type_text(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "type_text")?;
    let text = a.text.ok_or_else(|| anyhow::anyhow!("[chrome] type_text needs text"))?;
    forward(
        "tabs.type_text",
        json!({ "tabId": tab, "text": text, "delayMs": a.delay_ms.unwrap_or(0) }),
    )
    .await
}

async fn hover(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "hover")?;
    let sel = a.selector.ok_or_else(|| anyhow::anyhow!("[chrome] hover needs selector"))?;
    forward("tabs.hover", json!({ "tabId": tab, "selector": sel })).await
}

async fn select_option(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "select_option")?;
    let sel = a.selector.ok_or_else(|| anyhow::anyhow!("[chrome] select_option needs selector"))?;
    if a.value.is_none() && a.label.is_none() {
        return Ok(CallToolResult::error("[chrome] select_option needs value or label"));
    }
    forward(
        "tabs.select_option",
        json!({ "tabId": tab, "selector": sel, "value": a.value, "label": a.label }),
    )
    .await
}

async fn fill_form(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "fill_form")?;
    let fields = a.fields.ok_or_else(|| anyhow::anyhow!("[chrome] fill_form needs fields"))?;
    forward("tabs.fill_form", json!({ "tabId": tab, "fields": fields })).await
}

async fn drag(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "drag")?;
    let from = a.from_selector.ok_or_else(|| anyhow::anyhow!("[chrome] drag needs from_selector"))?;
    let to = a.to_selector.ok_or_else(|| anyhow::anyhow!("[chrome] drag needs to_selector"))?;
    forward(
        "tabs.drag",
        json!({ "tabId": tab, "fromSelector": from, "toSelector": to }),
    )
    .await
}

async fn upload_file(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "upload_file")?;
    let sel = a.selector.ok_or_else(|| anyhow::anyhow!("[chrome] upload_file needs selector"))?;
    let files = a.files.unwrap_or_default();
    if files.is_empty() {
        return Ok(CallToolResult::error("[chrome] upload_file needs files (array of paths)"));
    }
    forward(
        "tabs.upload_file",
        json!({ "tabId": tab, "selector": sel, "files": files }),
    )
    .await
}

async fn resize(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "resize")?;
    forward(
        "tabs.resize",
        json!({ "tabId": tab, "width": a.width, "height": a.height }),
    )
    .await
}

async fn emulate(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "emulate")?;
    forward(
        "tabs.emulate",
        json!({
            "tabId": tab,
            "viewport": a.viewport,
            "network": a.network,
            "userAgent": a.user_agent,
            "geolocation": a.geolocation,
            "timezone": a.timezone,
            "colorScheme": a.color_scheme,
            "cpuThrottling": a.cpu_throttling,
            "clear": a.clear.unwrap_or(false),
        }),
    )
    .await
}

async fn list_console_messages(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "list_console_messages")?;
    let r = chrome_bridge::call(
        "console.list",
        json!({
            "tabId": tab, "level": a.level, "contains": a.contains,
            "limit": a.limit.unwrap_or(100),
        }),
    )
    .await;
    match r {
        Ok(v) => {
            let entries = v.get("entries").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            let mut out = format!("# Console messages (showing {}/{})\n\n", entries.len(), total);
            for e in &entries {
                let lvl = e.get("level").and_then(|x| x.as_str()).unwrap_or("?");
                let ts = e.get("ts").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let body = if let Some(args) = e.get("args").and_then(|x| x.as_array()) {
                    args.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" ")
                } else {
                    e.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string()
                };
                out.push_str(&format!("- [{}][t={:.0}] {}\n", lvl, ts, body));
            }
            if entries.is_empty() {
                out.push_str("_(buffer empty — capture only starts after a console.* call has fired)_\n");
            }
            Ok(CallToolResult::text(out))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] list_console_messages: {}", e))),
    }
}

async fn clear_console(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "clear_console")?;
    forward("console.clear", json!({ "tabId": tab })).await
}

async fn handle_dialog(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "handle_dialog")?;
    let accept = a.accept.unwrap_or(true);
    forward(
        "dialog.handle",
        json!({ "tabId": tab, "accept": accept, "promptText": a.prompt_text.unwrap_or_default() }),
    )
    .await
}

async fn list_pending_dialogs(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "list_pending_dialogs")?;
    forward("dialog.list_pending", json!({ "tabId": tab })).await
}

async fn start_trace(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "start_trace")?;
    forward(
        "perf.start_trace",
        json!({ "tabId": tab, "categories": a.categories }),
    )
    .await
}

async fn stop_trace(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "stop_trace")?;
    let r = chrome_bridge::call("perf.stop_trace", json!({ "tabId": tab })).await;
    match r {
        Ok(v) => {
            let body = v.get("traceJson").and_then(|x| x.as_str()).unwrap_or("");
            let path = a.save_path.map(std::path::PathBuf::from).unwrap_or_else(|| {
                default_artifact_path("chrome-trace", "json")
            });
            if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
            if let Err(e) = std::fs::write(&path, body) {
                return Ok(CallToolResult::error(format!("[chrome] stop_trace write: {}", e)));
            }
            Ok(CallToolResult::text(format!(
                "[chrome] trace saved: {} ({} bytes)",
                path.display(),
                body.len()
            )))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] stop_trace: {}", e))),
    }
}

async fn heap_snapshot(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "heap_snapshot")?;
    let r = chrome_bridge::call("perf.heap_snapshot", json!({ "tabId": tab })).await;
    match r {
        Ok(v) => {
            let body = v.get("snapshot").and_then(|x| x.as_str()).unwrap_or("");
            let path = a.save_path.map(std::path::PathBuf::from).unwrap_or_else(|| {
                default_artifact_path("chrome-heap", "heapsnapshot")
            });
            if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
            if let Err(e) = std::fs::write(&path, body) {
                return Ok(CallToolResult::error(format!("[chrome] heap_snapshot write: {}", e)));
            }
            Ok(CallToolResult::text(format!(
                "[chrome] heap snapshot saved: {} ({} bytes) — open in Chrome DevTools → Memory → Load",
                path.display(),
                body.len()
            )))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] heap_snapshot: {}", e))),
    }
}

async fn lighthouse_audit(a: Args) -> Result<CallToolResult> {
    let tab = need_tab(&a, "lighthouse_audit")?;

    // Locate `lighthouse` on PATH. If absent, surface a friendly install hint
    // — we never bundle Lighthouse (it's huge).
    let lh_path = match which_binary("lighthouse").await {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(
                "[chrome] lighthouse_audit: `lighthouse` CLI not found on PATH. Install with `npm install -g lighthouse` (Node.js 18+ required).",
            ));
        }
    };

    // Resolve the tab's current URL via `tabs.list` so we audit what the user
    // is actually looking at, not whatever the LLM thinks is loaded.
    let url = match lookup_tab_url(tab).await {
        Some(u) if !u.is_empty() => u,
        _ => {
            return Ok(CallToolResult::error(format!(
                "[chrome] lighthouse_audit: could not resolve URL for tab_id {} (is the tab still open?)",
                tab
            )));
        }
    };

    // Validate categories — keep the allowlist tight.
    let allowed = ["performance", "accessibility", "best-practices", "seo", "pwa"];
    let raw_cats = a
        .categories
        .as_deref()
        .unwrap_or("performance,accessibility,best-practices,seo");
    let mut cats: Vec<String> = Vec::new();
    for piece in raw_cats.split(',') {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if !allowed.contains(&p) {
            return Ok(CallToolResult::error(format!(
                "[chrome] lighthouse_audit: unknown category `{}`. Allowed: {}",
                p,
                allowed.join(",")
            )));
        }
        cats.push(p.to_string());
    }
    if cats.is_empty() {
        cats = vec![
            "performance".into(),
            "accessibility".into(),
            "best-practices".into(),
            "seo".into(),
        ];
    }
    let cats_arg = cats.join(",");

    // Pick the output path — default to ~/.glance/cache/lighthouse-<ts>.html.
    let path = match a.save_path {
        Some(p) => PathBuf::from(p),
        None => default_artifact_path("lighthouse", "html"),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Run lighthouse with a 90s wall clock budget. Use --quiet to keep stdout
    // small (we mainly care about exit code + the saved file).
    let mut cmd = tokio::process::Command::new(&lh_path);
    cmd.arg(&url)
        .arg("--output=html")
        .arg(format!("--output-path={}", path.display()))
        .arg(format!("--only-categories={}", cats_arg))
        .arg("--quiet")
        .arg("--chrome-flags=--headless");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(CallToolResult::error(format!(
                "[chrome] lighthouse_audit: spawn failed: {}",
                e
            )));
        }
    };
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(90),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(CallToolResult::error(format!(
                "[chrome] lighthouse_audit: process error: {}",
                e
            )));
        }
        Err(_) => {
            return Ok(CallToolResult::error(
                "[chrome] lighthouse_audit: timed out after 90s",
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_short = if stderr.len() > 4000 {
            format!("{}…(truncated)", &stderr[..4000])
        } else {
            stderr.into_owned()
        };
        return Ok(CallToolResult::error(format!(
            "[chrome] lighthouse_audit: exit {} — {}",
            output.status.code().unwrap_or(-1),
            stderr_short.trim()
        )));
    }

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout_tail = stdout.trim();
    let stdout_short = if stdout_tail.len() > 1000 {
        format!("{}…(truncated)", &stdout_tail[..1000])
    } else {
        stdout_tail.to_string()
    };
    let mut out = format!(
        "[chrome] lighthouse_audit OK\n  url:        {}\n  categories: {}\n  saved:      {} ({} bytes)\n",
        url,
        cats_arg,
        path.display(),
        bytes
    );
    if !stdout_short.is_empty() {
        out.push_str("\nstdout:\n");
        out.push_str(&stdout_short);
        out.push('\n');
    }
    Ok(CallToolResult::text(out))
}

/// Tiny `which` shim — lighthouse is a Node.js shebang script so we only need
/// it on PATH. Returns `None` if not found.
async fn which_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn default_artifact_path(prefix: &str, ext: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    home.join(".glance/cache").join(format!("{}-{}.{}", prefix, ts, ext))
}

// ----- v0.42 adapter actions -----

async fn list_adapters(_a: Args) -> Result<CallToolResult> {
    let adapters = match crate::install::chrome_adapters::load_all() {
        Ok(m) => m,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] list_adapters: {}", e))),
    };
    let mut out = String::from("# Chrome adapters\n\n");
    if adapters.is_empty() {
        out.push_str("_(no adapters yet — write a YAML in `~/.glance/chrome-adapters/`, or use the Glance app's Chrome tab.)_\n");
        return Ok(CallToolResult::text(out));
    }
    out.push_str("| name | description | match_url | args |\n|---|---|---|---|\n");
    for a in adapters.values() {
        let desc = a.description.as_deref().unwrap_or("");
        let mu = a.match_url.as_deref().unwrap_or("—");
        let args = if a.args.is_empty() {
            "—".into()
        } else {
            a.args
                .iter()
                .map(|x| if x.required { format!("**{}**", x.name) } else { x.name.clone() })
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!("| `{}` | {} | `{}` | {} |\n", a.name, desc, mu, args));
    }
    out.push_str("\nInvoke with `run_adapter {name: \"...\", args: {...}}`. Pass `tab_id` explicitly, or omit it and the adapter will pick the first tab whose URL matches `match_url`.\n");
    Ok(CallToolResult::text(out))
}

async fn run_adapter(a: Args) -> Result<CallToolResult> {
    let name = match a.name {
        Some(v) => v,
        None => return Ok(CallToolResult::error("[chrome] run_adapter needs name")),
    };
    let adapters = match crate::install::chrome_adapters::load_all() {
        Ok(m) => m,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] run_adapter load: {}", e))),
    };
    let adapter = match adapters.get(&name) {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error(format!("[chrome] no adapter named: {}", name))),
    };

    // Validate / fill args.
    let args_in = a.args.unwrap_or_else(|| json!({}));
    let mut merged = serde_json::Map::new();
    if let Some(obj) = args_in.as_object() {
        for (k, v) in obj { merged.insert(k.clone(), v.clone()); }
    }
    for spec in &adapter.args {
        if !merged.contains_key(&spec.name) {
            if let Some(d) = &spec.default {
                merged.insert(spec.name.clone(), d.clone());
            } else if spec.required {
                return Ok(CallToolResult::error(format!(
                    "[chrome] adapter `{}` requires arg `{}`",
                    name, spec.name
                )));
            }
        }
    }
    let args_value = Value::Object(merged);

    // Pick a tab.
    let tab_id = if let Some(t) = a.tab_id {
        t
    } else if let Some(pat) = &adapter.match_url {
        match pick_tab_by_url(pat).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Ok(CallToolResult::error(format!(
                    "[chrome] adapter `{}`: no open tab matches {}",
                    name, pat
                )));
            }
            Err(e) => return Ok(CallToolResult::error(format!("[chrome] tab lookup: {}", e))),
        }
    } else {
        return Ok(CallToolResult::error(format!(
            "[chrome] adapter `{}` has no match_url; pass `tab_id` explicitly",
            name
        )));
    };

    let script = match crate::install::chrome_adapters::build_invocation_script(&adapter, &args_value) {
        Ok(s) => s,
        Err(e) => return Ok(CallToolResult::error(format!("[chrome] build script: {}", e))),
    };
    let world = adapter
        .world
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "main".into());
    let r = chrome_bridge::call(
        "tabs.evaluate",
        json!({
            "tabId": tab_id,
            "expression": script,
            "awaitPromise": adapter.await_promise,
            "world": world,
        }),
    )
    .await;
    match r {
        Ok(v) => {
            let s = if v.is_string() {
                v.as_str().unwrap().to_string()
            } else {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            };
            Ok(CallToolResult::text(format!(
                "<!-- adapter={} tab={} -->\n{}",
                name, tab_id, s
            )))
        }
        Err(e) => Ok(CallToolResult::error(format!("[chrome] run_adapter: {}", e))),
    }
}

async fn pick_tab_by_url(pattern: &str) -> Result<Option<i64>> {
    let v = chrome_bridge::call("tabs.list", json!({})).await?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let re = regex::Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("invalid match_url regex {}: {}", pattern, e))?;
    for t in arr {
        let url = t.get("url").and_then(|x| x.as_str()).unwrap_or("");
        if re.is_match(url) {
            return Ok(t.get("id").and_then(|x| x.as_i64()));
        }
    }
    Ok(None)
}
