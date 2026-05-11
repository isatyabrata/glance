# glance

> An MCP server that delegates **research / markdown / obsidian / chrome** work to a
> cheap sub-agent (GLM, DeepSeek, OpenAI, …) so codex / claude / cursor save
> tokens on the heavy "read N files and figure out what's there" loop.

[![License: PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm%20NC%201.0.0-orange)](LICENSE)
[![status](https://img.shields.io/badge/status-v0.61-blue)](#status)

[中文](README.md)

## Why

When you ask codex "find the login validation logic", codex itself reads a
dozen files into its 200K context window. By turn 30 the context is full and
the conversation gets shallow.

`glance` puts a **sub-agent** between codex and the codebase:

```text
codex                   ← keeps thinking, writes the actual code
  │ tool_call: research(query="find login validation logic")
  ▼
glance MCP server       ← receives task, drives a cheap model
  │
  ▼
GLM / DeepSeek          ← reads the files, greps, summarizes
  ▲
  │ "src/auth/login.ts:45-120 is core. validate.ts is its dep.
  │  Current flow: username/pwd → bcrypt.compare → JWT. Suggest line 70."
  │
codex                   ← gets a 200-token summary instead of 8KB of raw files
```

In a typical session this saves **40-80% of codex tokens** and roughly triples
the number of tasks you can run inside a 5-hour rate window. The cheap model
costs cents.

## Status

18 tools + a 35-action Chrome bridge + 70+ optional site adapters.

| Version | What was added |
|---|---|
| **v0.41** | Chrome bridge surface alignment — became a strict superset of [`@playwright/mcp`](https://github.com/microsoft/playwright-mcp) ∪ [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp). 28 chrome actions: input / wait / network capture / console / dialog / emulate / heap snapshot / start_trace ... |
| **v0.42** | YAML site adapters: `~/.glance/chrome-adapters/<name>.yaml` that codify frequent queries, `run_adapter` returns structured JSON in one shot. GUI includes YAML editor with pre-save validation + new / delete / Finder jump |
| **v0.43** | Native messaging host rewritten in Rust — dropped the Node dependency. `cargo install` directly puts `glance-chrome-host` on PATH |
| **v0.44** | `lighthouse_audit` action (shells out to `lighthouse` CLI) + "save last evaluate as adapter" auto-capture in GUI |
| **v0.45** | `glance chrome import` — translates [opencli](https://github.com/jackwener/opencli) / [autocli](https://github.com/nashsu/autocli) YAML site adapters into the glance schema |
| **v0.46** | Vendored 65 autocli adapters into `examples/chrome-adapters/` (Apache-2.0 attribution preserved). `glance chrome import examples/chrome-adapters/` installs 70 adapters in one line (5 starter + 65 vendored). Covers 27 sites: Douban / Bilibili / Reddit / Xueqiu / Twitter / TikTok / Instagram / Facebook / Notion / Medium / linux.do / Zhihu / Weibo / Jike / Boss / barchart / Xiaohongshu / v2ex / Substack / Sina Blog / Reuters / Ctrip / Chaoxing / Bloomberg ... |
| **v0.47** | Breached strict CSP + rich text editors. `evaluate` gains `world: "cdp"` mode using `chrome.debugger.Runtime.evaluate` to bypass page CSP via debugger privilege (sites like X.com that block `unsafe-eval`). New `set_contenteditable` action running pre-compiled functions in ISOLATED world, uses `execCommand('insertText')` + InputEvent fallback to feed Draft.js / Lexical / ProseMirror. Bundled `x_post` adapter as a working example |
| **v0.48** | `paste_text` action: DataTransfer + ClipboardEvent("paste") dispatch, ISOLATED world pre-compiled function. Fixed `press_key` Cmd+A bug (no longer emits stray "a" with modifiers). Bridge timeout 15s→30s |
| **v0.49** | Three features landed simultaneously: (1) Chrome tab group control panel (Codex `@chrome`-style purple pill) — manifest added `tabGroups` permission, each driven tab auto-grouped under `Glance · <method>`, 5min idle auto-ungroup; (2) `click_native` / `fill_native` moved to ISOLATED world, immune to page CSP; (3) `os_paste` action — pbcopy + CDP `Input.dispatchKeyEvent commands:["paste"]`, OS-level real paste bypasses Draft.js isTrusted checks |
| **v0.50** | chrome.debugger auto-detaches after 60s idle. The `"started debugging this browser"` yellow banner no longer persists — only shows during actual CDP moments |
| **v0.51** | `submit_post` action — focuses editor + CDP Cmd+Enter, trusted keyboard event, stable submission for X / Bluesky / any editor that recognizes Cmd+Enter. Replaces synthetic `dispatchEvent(MouseEvent)` clicks (isTrusted=false silently rejected by X) |
| **v0.52** | Visual mouse SVG overlay: before each `click_native` / `hover`, inline-injects a purple cursor SVG that flies to the target, 250ms settle so the user can see it, 600ms fade out. `pointer-events:none` doesn't interfere with real operations. Codex `@chrome`-equivalent UX |
| **v0.53** | `click_native` also upgraded to CDP `Input.dispatchMouseEvent` — earlier synthetic clicks were rejected by X's SideNav Post button (isTrusted=false silently changed URL without opening modal). OS-level trusted clicks now open modals correctly |
| **v0.54** | `type_multiline` action — multi-paragraph rich text insertion **without stealing OS focus**. CDP `Input.insertText` for whole chunks + CDP Enter between paragraphs + 600ms React commit wait. Completely solves the problem of writing multi-paragraph text to Draft.js while Chrome is in the background (os_paste requires Chrome window in foreground to read system clipboard; type_multiline doesn't) |
| **v0.55** | One-shot `tweet` action — bakes the entire X posting flow into a single MCP call: `Emulation.setFocusEmulationEnabled` + click SideNav wait-for-modal + `type_multiline` + optional `upload_file` + 4s review + `submit_post` + URL capture. LLM posts in one line: `chrome { action: "tweet", value, files? }`. The repo's 22-tweet thread was entirely posted with this |
| **v0.56** | `sub_agent` gained deadline_secs (90s) + chat_timeout_secs (45s) wall-clock guards. When a single GLM call hangs or total budget is exceeded, returns a partial summary with `[glance partial: ...]` suffix so the main model immediately knows to narrow scope or fall back. v0.60 adjusted chat_timeout 25→45s to tolerate first cold calls |
| **v0.57** | Upstream MCP call timeout 60s→100s (enough for large MySQL aggregations), transport gained 110s wall-clock total gate — no call path ever makes the MCP client wait the full 120s |
| **v0.58** | Prompt cache hit rate visibility. `Usage` uses custom Deserialize to normalize parsing of three cache fields (OpenAI `prompt_tokens_details.cached_tokens` / Anthropic `cache_read_input_tokens` / DeepSeek `prompt_cache_hit_tokens`), accumulated to event log via CallCtx. `ChatRequest` field order changed to `tools → messages` to align with Anthropic prefix caching. Menubar Usage panel shows `cache_hit_rate = glm_cached_tokens / glm_prompt_tokens` |
| **v0.59** | `evaluate` wrapper triple-fix: (1) IIFE expressions no longer double-wrapped (preserved single-expression `return (...)` auto-wrap); (2) non-serializable return values (DOM nodes / Function / Promise) now return `{__glance_eval__, type, subtype, className, hint}` diagnostic object instead of silent null; (3) new `raw: true` parameter completely skips wrapping, supports multi-statement scripts with self-managed returns |
| **v0.60** | sub_agent 0-iter partial now precise: previously always said "narrow the scope", now at 0 iters says "GLM backend slow / unreachable — fall back to local Grep / Read". chat_timeout 25→45s default to leave headroom for TLS handshake on cold start |
| **v0.61** | Chrome: swallow MV3 service worker lifecycle races (service worker may terminate between calls; bridge now retries with backoff and reconnects transparently) |

## Tools

| | Tool | Purpose | Side effects |
|---|---|---|---|
| ✅ | `research` | Read multiple files / grep / summarize | none |
| ✅ | `explain` | Explain a file / function / chunk | none |
| ✅ | `search` | Pattern / semantic search (regex or grep-bootstrapped) | none |
| ✅ | `md_read` | Read a markdown file (frontmatter + body split) | none |
| ✅ | `md_outline` | Return heading tree of a markdown file | none |
| ✅ | `md_write` | Generate proposed markdown content as a patch | patch |
| ✅ | `obsidian_read` | Read a note (frontmatter / wikilinks / tags parsed) | none |
| ✅ | `obsidian_search` | Full-text search a vault | none |
| ✅ | `obsidian_backlinks` | All notes linking to a given note | none |
| ✅ | `obsidian_write` | Create or modify a note | patch |
| ✅ | `write_tests` | Generate unit tests as a patch | patch |
| ✅ | `write_docs` | Generate docstrings / README as a patch | patch |
| ✅ | `fix_lint` | Lint / format fixes as a patch | patch |
| ✅ | `web_fetch` | Fetch URL → readability extraction (**0 LLM tokens**) | none |
| ✅ | `repo_explore` | GitHub API: structure / search_doc / read_file | none |
| ✅ | `image_describe` | Describe a local image via GLM-4.5V (cheap vision, saves Anthropic vision tokens) | none |
| ✅ | `web_search` | GLM `web_search_prime` (billed to GLM plan, not Anthropic) | none |
| ⚙️ | `chrome` | Drive your logged-in Chrome (reuses cookies / extensions / tabs) | browser |

Read tools default to **on**, write tools default to **off** — opt-in
explicitly. **Patch mode** = the sub-agent never touches disk; it emits a
proposed change to `~/.glance/patches/<ts>-<tool>-<basename>.patch` for the
calling model (codex / claude / cursor) to review and apply.

### Setup notes for gateway tools

- **`repo_explore search_doc`** uses GitHub's code-search API which **rejects
  anonymous requests**. Set `GITHUB_TOKEN` (a fine-grained personal access
  token with `public_repo` read scope is enough):

  ```bash
  echo 'export GITHUB_TOKEN=ghp_…' >> ~/.zshrc   # or ~/.bashrc
  ```

  `structure` and `read_file` work without a token but get rate-limited at 60
  req/h per IP.

- **`image_describe`** hardcodes the vision model to `glm-4.5v`. Your
  `BackendConfig.api_key` must have access to that SKU (GLM Coding Plan does).

- **`web_search`** routes through GLM's MCP web-search endpoint over
  Streamable HTTP. Counts against your GLM Coding Plan's MCP quota
  (1000/month combined for Pro, 4000/month for Max). No setup beyond a valid
  `api_key`.

## Install

```bash
cargo install --git https://github.com/isatyabrata/glance
```

This installs both `glance` (CLI) and `glance-mcp` (MCP server) into
`~/.cargo/bin/`. (crates.io and homebrew releases land once the API stabilizes.)

Then register with all clients in one command:

```bash
glance install        # auto-registers with codex / claude code / cursor
glance doctor         # check backend / client registration / obsidian vault
```

## Configure

Create `~/.glance/config.toml`:

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key  = "sk-..."
model    = "glm-5.1"

[obsidian]
# optional; project-level AGENTS.md / CLAUDE.md can also declare:
#   mcp.obsidian_vault: /path/to/Vault
vault = "/path/to/your/Obsidian/Vault"
```

Or via env vars: `GLANCE_API_KEY` / `GLANCE_BASE_URL` / `GLANCE_MODEL`.

### Peak-hour 429 auto-degradation + retry

GLM Coding Plan frequently returns 429 during peak hours. glance has built-in:
retry N times per model (default 3, with exponential backoff 1s / 3s / 9s,
respecting `Retry-After` header), then **auto-fallback** to the next model in
`fallback_models`, each with its own retry budget.

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key  = "..."
model    = "GLM-4.5-air"                      # primary model
fallback_models = ["GLM-5-Turbo", "GLM-4.7"]  # fall through if primary exhausted

[backend.retry]
max_retries     = 3       # attempts per model
base_backoff_ms = 1000    # base: 1s × 3^N
max_backoff_secs = 30     # Retry-After cap (seconds)
```

Worst case: 3 models × 3 retries ≈ 9 attempts + 36s cumulative backoff before
reporting an error to the caller (codex / claude). Glance.app's Logs tab shows
the trace of each degradation / retry (`model exhausted retries, falling through to next`).

### ⚠️ GLM Coding Plan endpoint

If you're using the **Zhipu GLM Coding Plan subscription** (not pay-as-you-go),
the standard `https://open.bigmodel.cn/api/paas/v4` endpoint will return
`code: 1113 余额不足或无可用资源包` — coding plan traffic is
restricted to its own dedicated endpoint:

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model    = "GLM-4.5-air"   # uppercase — coding plan endpoint is case-sensitive
```

Models under coding plan: `GLM-5.1` / `GLM-4.7` / `GLM-4.5-air` / `GLM-5-Turbo`.

## Wire to your client

`glance install` does all of this automatically. Manual setup if preferred:

### codex CLI

`~/.codex/config.toml`:

```toml
[mcp_servers.glance]
command = "glance-mcp"
startup_timeout_sec = 10
tool_timeout_sec = 120
```

### Claude Code

```bash
claude mcp add glance glance-mcp
```

Or hand-edit `~/.claude.json`:

```json
{
  "mcpServers": {
    "glance": { "type": "stdio", "command": "glance-mcp" }
  }
}
```

Optional — add a routing hint to `~/.claude/CLAUDE.md` telling the model to
prefer glance for token-saving tasks:

```markdown
- Read web pages → glance.web_fetch (not WebFetch)
- Search the web → glance.web_search (not WebSearch)
- Explore GitHub repos → glance.repo_explore (don't clone+grep)
- Look at images → glance.image_describe (don't let Claude see images directly)
- Cross-file research → glance.research (before editing code)
```

### Cursor

`~/.cursor/mcp.json`:

```json
{ "mcpServers": { "glance": { "command": "glance-mcp" } } }
```

## Backend compatibility

Anything that speaks **OpenAI-compatible** chat completions + function calling:

| Backend | base_url | Notes |
|---|---|---|
| **GLM (Zhipu) coding plan** | `https://open.bigmodel.cn/api/coding/paas/v4` | uppercase model names |
| **GLM (Zhipu) pay-as-you-go** | `https://open.bigmodel.cn/api/paas/v4` | lowercase model names |
| **DeepSeek** | `https://api.deepseek.com` | tested-by-design |
| **OpenAI** | `https://api.openai.com/v1` | works as upstream |
| **Local (Ollama / vLLM)** | `http://localhost:11434/v1` | works for any function-calling-capable local model |

Pick whatever's cheap. The sub-agent loop doesn't care which model is on the
other end as long as it supports OpenAI function calling.

## How it works

1. codex / claude / cursor calls e.g. `research(query="...", scope=[...])`
   over the MCP stdio protocol
2. `glance-mcp` receives the call, picks the right tool dispatcher
3. The tool builds a system prompt and calls `sub_agent::run(system, user)`
4. The sub-agent loop drives **your configured backend** (GLM/DeepSeek/...)
   with three internal tools: `read_file` (with offset/limit for big files),
   `list_dir`, and `grep`
5. The model loops up to `max_iterations` times, calling tools as it needs,
   until it returns a final text answer
6. Glance returns the final text to the MCP caller — **never the raw file
   content**

The big-file behavior matters: `read_file` returns a paginated window
(default 400 lines, max 2000) so the sub-agent can `grep` to locate
relevant sections of large files instead of choking on 100KB+ source files.

## GUI (optional)

`glance-app/` is a Tauri 2 menu-bar application with **Claude Design**
aesthetics, showing backend connectivity / daily call count / bytes saved /
tool toggles / live `events.jsonl` log / Obsidian vault selector. Installed
at `/Applications/Glance.app`, accessible from the menu bar icon.

Build:

```bash
cd glance-app
npm install
npx tauri build --bundles app
# output: target/release/bundle/macos/Glance.app
```

## Chrome bridge

The `chrome` tool lets Claude / Codex / Cursor **drive your currently
logged-in Chrome** — reusing all cookies / sessions / extensions / open tabs.
Unlike `mcp__chrome-devtools__*` / `playwright` which spawn a fresh browser,
this uses Chrome's own `chrome.scripting` + `chrome.debugger` APIs and does
**not** require `--remote-debugging-port`.

> **Hat tip to Codex**: The architecture is inspired by OpenAI Codex desktop's
> `@chrome` plugin — a Chrome extension + native message host + internal protocol.
> We reused no Codex code or extension (Codex's `extension-host` binary is locked
> to its own extension ID), but the three-piece architecture (extension ↔ native
> host ↔ MCP server) follows the same pattern. **Glance's implementation is
> completely independent and does not depend on Codex running.**

### Actions

| action | Purpose |
|---|---|
| `list_tabs` | List all tabs across all Chrome windows (id / title / url / active) |
| `navigate` | Navigate a tab to a URL |
| `wait_load` | Wait for tab to reach `complete` state (for post-navigation data capture) |
| `evaluate` | Run arbitrary JS in the tab's MAIN world, return result (with `await_promise`) |
| `click` | querySelector + dispatch mousedown/up/click events, correctly triggers SPA frameworks |
| `fill` | Set input/textarea value and dispatch input/change events |
| `screenshot` | Capture visible viewport, save as PNG/JPEG (default `~/.glance/cache/`) |
| `cdp` | Pass through any CDP command (`Page.captureScreenshot` / `Network.*` / `Runtime.*`) |

Plus 27 more actions: `click_native`, `fill_native`, `hover`, `press_key`,
`type_multiline`, `paste_text`, `os_paste`, `set_contenteditable`, `submit_post`,
`tweet`, `upload_file`, `run_adapter`, `lighthouse_audit`, and more.

### Install (4 steps, 3 minutes)

```bash
# 1. Copy extension + native host + write Chrome native messaging manifest
glance chrome install
```

This will:
- Place the extension in `~/.glance/chrome-bridge/extension/` (manifest hardcodes
  an RSA public key so Chrome computes a **deterministic** extension ID:
  `eofgbpadckhmkhhbbhekngmkgagfifhe`)
- Place the Rust native host in `~/.glance/chrome-bridge/host/`
- Write the native host manifest at `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.glance.chrome.json`
  with `allowed_origins` pointing to the fixed extension ID
- Auto-open `chrome://extensions` for you

```
# 2. In chrome://extensions, toggle "Developer mode" (top right)
# 3. Click "Load unpacked", select ~/.glance/chrome-bridge/extension/
#    Extension ID should automatically be eofgbpadckhmkhhbbhekngmkgagfifhe (determined by manifest key)
# 4. In Glance app's Tools tab, toggle chrome ON (or set tools.chrome = true in ~/.glance/config.toml)
```

Then **restart your Claude Code / Codex / Cursor session** so it re-fetches
`tools/list`.

`glance chrome status` shows the current binding state.

### Use cases

```
"Check what today's sales look like on the OMS page I have open"
→ chrome list_tabs → chrome evaluate (scrape table) → summarize

"Paste me the content of that LinuxDo post"
→ chrome list_tabs → chrome evaluate (innerText) → quote

"Open GitHub, search for zai codex, screenshot"
→ chrome navigate → chrome wait_load → chrome screenshot
```

### Why not chrome-devtools / playwright

Both are useful but differ:
- **chrome-devtools MCP** uses the CDP port, requires launching Chrome with
  `--remote-debugging-port=9222`, and exposes that port to all local processes
- **playwright** launches a **fresh** clean browser without your cookies,
  extensions, or login state
- **glance.chrome** uses a Chrome extension attached to `chrome.debugger` API,
  works with your **existing Chrome process** — developer mode only, no debug
  port open

### Hiding chrome from Codex

Codex ships its own `@chrome` plugin (same principle). If both run you'll
have collisions. Glance app → Tools tab → find the `chrome` row → uncheck the
`[codex]` pill.

## Importing third-party adapters

[opencli](https://github.com/jackwener/opencli) and its Rust port
[autocli](https://github.com/nashsu/autocli) collectively maintain 100+ site
YAML recipes (Douban, Bilibili, Notion, Twitter, Boss Zhipin...). Instead of
porting their entire runtime (weeks of work, more complex schema), glance
wrote a **translation layer**:

```bash
glance chrome import <yaml-file-or-dir>   # → ~/.glance/chrome-adapters/
glance chrome import examples/chrome-adapters/   # try the 5 bundled ones
```

The translator only accepts **browser DOM scraping** adapters (single
`evaluate:` step, running in your logged-in Chrome page). These are **skipped**
with a log line:

| Upstream field | Action | Why |
|---|---|---|
| `strategy: public` | skip | Pure HTTP fetch — use `glance.web_fetch` or curl the API directly |
| `strategy: intercept` / `auth: INTERCEPT` | skip | Requires network interception, glance.chrome doesn't do that |
| `auth: TOKEN` | skip | Requires token tracking across requests |
| pipeline with `fetch:` / `collect:` / `intercept:` steps | skip | Requires opencli runtime |
| multiple `evaluate:` steps | skip | We run one JS snippet at a time |

The rest auto-convert: `${{ args.foo }}` placeholders are inlined as `args.foo`,
the JS is wrapped to normalize return values as `{ rows: [...] }`, and a
`match_url` regex is inferred from `endpoint` / `domain` / `navigate:` fields.
After import, `chrome run_adapter {name, args}` works.

```bash
# Dry run (doesn't write to disk)
glance chrome import examples/chrome-adapters/ --dry-run

# Actually write to ~/.glance/chrome-adapters/
glance chrome import examples/chrome-adapters/

# Single file
glance chrome import path/to/some-autocli.yaml --force
```

The repo's `examples/chrome-adapters/` ships with 5 clean translations:
`douban_movie-hot` / `douban_book-hot` / `douban_top250` / `notion_sidebar` /
`bilibili_feed`. These are **not** auto-installed — DOM scraping selectors
break when sites redesign, so blindly preloading them creates landmines.
`import` when you need them, own them yourself.

> When to use a chrome adapter? Only when a site **requires login state** or
> **has no public API**. For public web content use `glance.web_fetch` — cheaper
> in tokens, no Chrome needed. For sites with public JSON/RSS APIs (Hacker News,
> dev.to, arXiv), fetch directly instead of going through chrome adapters.

Attribution: opencli / autocli are Apache-2.0. Each YAML in
`examples/chrome-adapters/` notes the upstream URL + commit SHA + original
author at the top. Translated copies in the glance repo inherit
PolyForm-Noncommercial (repo-wide license); the upstream Apache-2.0 NOTICE is
preserved in each file header.

## License

[PolyForm Noncommercial 1.0.0](LICENSE) — free for personal use, research,
hobby projects, and noncommercial organizations. Commercial use requires a
separate license — open an issue if interested.

## Acknowledgments

- Architecture (delegating to a sub-agent to save tokens) inspired by
  [CodexSaver](https://github.com/fendouai/CodexSaver) — same delegation idea,
  different backends, different toolset, independent codebase.
- Chrome bridge (v0.3) three-piece architecture (extension ↔ native host ↔
  MCP server) references the design of [OpenAI Codex desktop](https://openai.com/codex/)'s
  `@chrome` plugin; the implementation is fully independent and does not depend
  on Codex.
- Chrome parity tool surface (v0.41) benchmarked against
  [Microsoft Playwright MCP](https://github.com/microsoft/playwright-mcp) and
  [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp),
  filling in their unique actions so glance.chrome is a strict superset of both.
- YAML site adapter concept (v0.42) from
  [opencli](https://github.com/jackwener/opencli) /
  [autocli](https://github.com/nashsu/autocli) (100+ site recipes); schema is
  glance's own minimal subset, not a direct reuse of their runtime.
- `glance chrome import` translation layer (v0.43) converts upstream
  Apache-2.0 YAML to glance schema (DOM scraping only); 5 samples in
  `examples/chrome-adapters/` are labeled with original URL + commit.
- Menu-bar GUI's RTK tag + Token Killer hook integration from
  [rtk-ai/rtk](https://github.com/rtk-ai/rtk) — silently rewrites high-output
  commands like `git status` / `cargo test` as `rtk <cmd>`, saving 60-90% of
  tokens in AI agent context.
- CCUSAGE usage panel (daily / session breakdown) data sourced from
  [ccusage](https://github.com/ryoppippi/ccusage) — parses Claude Code's local
  billing sqlite into visual reports; glance shells out to its CLI.

---

**Forked from [xtftbwvfp/glance](https://github.com/xtftbwvfp/glance)** — all credit for the original architecture, tools, and Chrome bridge goes to the original author.
