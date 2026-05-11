# glance

> 一个 MCP 服务器，把 codex / claude / cursor 里"读 N 个文件再回答"这种重活
> 转给便宜的子模型（GLM / DeepSeek / OpenAI 都行），让贵的主模型省 token。

[![License: PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm%20NC%201.0.0-orange)](LICENSE)
[![status](https://img.shields.io/badge/status-v0.2-blue)](#%E7%8A%B6%E6%80%81)
[English](README.en.md)

## 为什么要它

你让 codex "找一下登录校验在哪写的"——codex 自己就把十几个文件读进它 200K 的
context 窗口里。一轮聊到第 30 轮，context 满了，回答开始变浅。

`glance` 在 codex 和代码之间塞了一个**子代理**：

```text
codex                   ← 继续思考、写真正的代码
  │ tool_call: research(query="找一下登录校验在哪")
  ▼
glance MCP 服务器       ← 接活，驱动一个便宜模型
  │
  ▼
GLM / DeepSeek          ← 读文件、grep、总结
  ▲
  │ "src/auth/login.ts:45-120 是核心。validate.ts 是它的依赖。
  │  当前流程：username/pwd → bcrypt.compare → JWT。建议改 70 行。"
  │
codex                   ← 拿到 200 token 的摘要，而不是 8KB 的原始文件
```

一个普通 session 下来能省 **40-80% 的 codex token**，5 小时窗口里能跑的任务
量大致 ×3。子模型那边一次几分钱。

## 状态

18 个工具 + 一个 35-action 的 Chrome 桥 + 70+ 个可选站点适配器。

| 版本 | 加了什么 |
|---|---|
| **v0.41** | Chrome 桥 surface 对齐——成为 [`@playwright/mcp`](https://github.com/microsoft/playwright-mcp) ∪ [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp) 的严格超集。28 个 chrome action：input / wait / network 抓包 / console / dialog / emulate / heap snapshot / start_trace ... |
| **v0.42** | YAML 站点适配器：`~/.glance/chrome-adapters/<name>.yaml` 把高频查询固化，`run_adapter` 一击拿结构化 JSON。GUI 内置 YAML 编辑器（保存前 parse 校验）+ 新建 / 删除 / Finder 跳转 |
| **v0.43** | Native messaging host 用 Rust 重写——掉 Node 依赖。`cargo install` 直接铺 `glance-chrome-host` 到 PATH |
| **v0.44** | `lighthouse_audit` action（shell out 到 `lighthouse` CLI）+ "保存最近 evaluate 为适配器" GUI 自动捕获 |
| **v0.45** | `glance chrome import` —— 从 [opencli](https://github.com/jackwener/opencli) / [autocli](https://github.com/nashsu/autocli) 把 YAML 站点适配器翻译成 glance schema |
| **v0.46** | Vendor 65 条 autocli 适配器到 `examples/chrome-adapters/`（Apache-2.0 attribution 完整保留）。`glance chrome import examples/chrome-adapters/` 一行装上 70 条（5 starter + 65 vendored）。覆盖 27 个站点：豆瓣 / B 站 / Reddit / 雪球 / Twitter / TikTok / Instagram / Facebook / Notion / Medium / linux.do / 知乎 / 微博 / 即刻 / Boss / barchart / 小红书 / v2ex / Substack / 新浪博客 / 路透 / 携程 / 超星 / Bloomberg ...   |
| **v0.47** | 攻破严格 CSP + 富文本编辑器。`evaluate` 加 `world: "cdp"` 走 `chrome.debugger.Runtime.evaluate`，靠 debugger 特权绕开页面 CSP（X.com 等阻 `unsafe-eval` 站）。新 action `set_contenteditable` 在 ISOLATED world 跑预编译 func，靠 `execCommand('insertText')` + InputEvent fallback 喂 Draft.js / Lexical / ProseMirror。打包内置 `x_post` 适配器作为现成范例 |
| **v0.48** | `paste_text` action：DataTransfer + ClipboardEvent("paste") 分发，ISOLATED world 预编译 func。修 `press_key` 的 Cmd+A bug（带 modifier 时不再误吐 "a"）。桥 timeout 15s→30s |
| **v0.49** | 三件套同时落地：(1) Chrome tab group 控制面板（Codex `@chrome` 风格的紫色 pill）—— manifest 加 `tabGroups` 权限，每个被驱动的 tab 自动并入 `Glance · <method>` 组，5min 空闲自动解散；(2) `click_native` / `fill_native` 改 ISOLATED world，免受页面 CSP；(3) `os_paste` action —— pbcopy + CDP `Input.dispatchKeyEvent commands:["paste"]`，OS 级真粘贴绕过 Draft.js isTrusted 检查 |
| **v0.50** | chrome.debugger 闲置 60s 自动 detach。`"started debugging this browser"` 黄条不再持续显示——只在真正用 CDP 那一刻弹出 |
| **v0.51** | `submit_post` action —— focus 编辑器 + CDP Cmd+Enter，trusted keyboard event，X / Bluesky / 任何识 Cmd+Enter 的编辑器都能稳提交。代替 `dispatchEvent(MouseEvent)` 的合成 click（isTrusted=false 会被 X 静默拒绝） |
| **v0.52** | 视觉鼠标 SVG overlay：每次 `click_native` / `hover` 前 inline 注入紫色光标 SVG 飞到目标，250ms settle 让用户能看见，600ms 淡出。`pointer-events:none` 不干扰真操作。Codex `@chrome` 同款 UX |
| **v0.53** | `click_native` 也升 CDP `Input.dispatchMouseEvent` —— 早先合成 click 被 X 的 SideNav Post 按钮拒（isTrusted=false 静默改 URL 不弹 modal）。改 OS 级 trusted click 之后模态正常弹出 |
| **v0.54** | `type_multiline` action —— **不抢 OS 焦点**的多段富文本插入。CDP `Input.insertText` 整段塞 + 段间 CDP Enter 键 + 600ms React 提交等待。彻底解决 Chrome 后台时往 Draft.js 写多段文字的问题（os_paste 需要 Chrome 窗口在前才能读系统剪贴板，type_multiline 不需要） |
| **v0.55** | One-shot `tweet` action —— 把整套 X 发推流程烧成一个 MCP 调用：`Emulation.setFocusEmulationEnabled` + 点 SideNav 等模态 + `type_multiline` + 可选 `upload_file` + 4s review + `submit_post` + URL 捕获。LLM 一行 `chrome { action: "tweet", value, files? }` 一条推。本仓库的 22 推 thread 全靠这个发出 |
| **v0.56** | `sub_agent` 加 deadline_secs (90s) + chat_timeout_secs (45s) wall-clock guard。GLM 单次调用卡死或整体爆 budget 时返回带 `[glance partial: ...]` 后缀的部分摘要，主模型立刻知道该 narrow scope 或 fallback。v0.60 调整 chat_timeout 25→45s 容忍首次冷调用 |
| **v0.57** | 上游 MCP 调用 timeout 60s→100s（MySQL 大聚合够用），transport 加 110s wall-clock 总闸——任何调用路径都不会让 MCP 客户端等满 120s |
| **v0.58** | 提示词缓存命中率可见化。`Usage` 用 custom Deserialize 归一化解析三种缓存字段（OpenAI `prompt_tokens_details.cached_tokens` / Anthropic `cache_read_input_tokens` / DeepSeek `prompt_cache_hit_tokens`），通过 CallCtx 累加到事件日志。`ChatRequest` 字段顺序改 `tools → messages` 对齐 Anthropic 前缀缓存。menubar Usage 面板可直接看 `cache_hit_rate = glm_cached_tokens / glm_prompt_tokens` |
| **v0.59** | `evaluate` 封装三修：(1) IIFE 表达式不再双重 wrap（保留单表达式的 `return (...)` 自动 wrap）；(2) 非可序列化返回值（DOM 节点 / Function / Promise）改返 `{__glance_eval__, type, subtype, className, hint}` 诊断对象代替静默 null；(3) 新 `raw: true` 参数完全跳过 wrap，支持多语句脚本自管返回 |
| **v0.60** | sub_agent 0-iter partial 精准提示：之前一律说 "narrow the scope"，现在 0 iters 时改为 "GLM backend slow / unreachable — fall back to local Grep / Read"。chat_timeout 25→45s 默认值，给冷启动留出 TLS handshake 余量 |

## 工具清单

| | 工具 | 用途 | 副作用 |
|---|---|---|---|
| ✅ | `research` | 读多个文件 / grep / 总结 | 无 |
| ✅ | `explain` | 解释一个文件 / 函数 / 代码片段 | 无 |
| ✅ | `search` | 模式 / 语义搜索（regex 或 grep 引导） | 无 |
| ✅ | `md_read` | 读 markdown 文件（frontmatter / body 拆开） | 无 |
| ✅ | `md_outline` | 返回 markdown 文件的标题树 | 无 |
| ✅ | `md_write` | 生成 markdown 内容（patch 模式） | patch |
| ✅ | `obsidian_read` | 读 Obsidian 笔记（frontmatter / wikilinks / tags） | 无 |
| ✅ | `obsidian_search` | 全文搜索 vault | 无 |
| ✅ | `obsidian_backlinks` | 列出反向链接 | 无 |
| ✅ | `obsidian_write` | 创建 / 修改笔记 | patch |
| ✅ | `write_tests` | 生成单元测试（patch 模式） | patch |
| ✅ | `write_docs` | 生成 docstring / README（patch 模式） | patch |
| ✅ | `fix_lint` | lint / 格式化修复（patch 模式） | patch |
| ✅ | `web_fetch` | 抓 URL → readability 抽正文（**0 LLM token**） | 无 |
| ✅ | `repo_explore` | GitHub API：structure / search_doc / read_file | 无 |
| ✅ | `image_describe` | 用 GLM-4.5V 描述本地图片（替代贵的视觉 token） | 无 |
| ✅ | `web_search` | GLM `web_search_prime`（计入 GLM 套餐，不是 Anthropic） | 无 |
| ⚙️ | `chrome` | 驱动你当前登录的 Chrome（复用 cookie / 扩展 / 标签页） | 浏览器 |

只读工具默认**开**，写入类工具默认**关**——要用得显式打开。
**Patch 模式** = 子代理永远不直接改你的磁盘，它把改动写成 patch 文件
`~/.glance/patches/<时间戳>-<工具>-<basename>.patch`，让调用它的模型
（codex / claude / cursor）自己审过再 apply。

### v0.2 网关工具的环境配置

- **`repo_explore search_doc`** 用 GitHub 代码搜索 API，**不能匿名**。要在
  环境变量里设 `GITHUB_TOKEN`（fine-grained PAT，`public_repo` 读权限就够）：

  ```bash
  echo 'export GITHUB_TOKEN=ghp_…' >> ~/.zshrc   # 或 ~/.bashrc
  ```

  `structure` 和 `read_file` 不带 token 也能跑，但 GitHub 限速 60 req/h/IP。

- **`image_describe`** 视觉模型固定写死 `glm-4.5v`。你的
  `BackendConfig.api_key` 必须能调用这个 SKU（GLM Coding Plan 是可以的）。

- **`web_search`** 走 GLM 的 MCP web-search 端点（Streamable HTTP 协议），
  计入你 GLM Coding Plan 的 MCP 配额（Pro 套餐 1000 次/月共享，Max 4000
  次/月）。除了 `api_key` 不需要别的配置。

## 安装

```bash
cargo install --git https://github.com/xtftbwvfp/glance
```

会把 `glance` (CLI) 和 `glance-mcp` (MCP server) 装到 `~/.cargo/bin/`。
（crates.io / homebrew 等 API 稳定后再发。）

然后一键写入所有客户端配置：

```bash
glance install        # 自动注册到 codex / claude code / cursor
glance doctor         # 检查 backend / 各客户端注册状态 / Obsidian vault
```

## 配置

新建 `~/.glance/config.toml`：

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key  = "sk-..."
model    = "glm-5.1"

[obsidian]
# 可选；项目里的 AGENTS.md / CLAUDE.md 也可以声明：
#   mcp.obsidian_vault: /path/to/Vault
vault = "/path/to/your/Obsidian/Vault"
```

或者走环境变量：`GLANCE_API_KEY` / `GLANCE_BASE_URL` / `GLANCE_MODEL`。

### 高峰期 429 自动降级 + 重试

GLM Coding Plan 高峰常返 429。glance 内置：每个模型重试 N 次（默认 3，
带指数退避 1s / 3s / 9s，尊重 `Retry-After` 头），用尽后**自动降级**到
`fallback_models` 里下一个模型，每级独享自己的重试预算。

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
api_key  = "..."
model    = "GLM-4.5-air"               # 主模型
fallback_models = ["GLM-5-Turbo", "GLM-4.7"]   # 主挂了走这两个

[backend.retry]
max_retries     = 3       # 每个模型重试几次
base_backoff_ms = 1000    # 退避基数：1s × 3^N
max_backoff_secs = 30     # Retry-After 上限（秒）
```

最坏情况：3 个模型 × 3 次 ≈ 9 次尝试 + 累计 36s 退避，仍失败才向调用方
（codex / claude）报错。Glance.app 日志 tab 能看到每次降级 / 重试的痕迹
（`model exhausted retries, falling through to next`）。

### ⚠️ GLM Coding Plan 端点不一样

如果你用的是**智谱 GLM Coding Plan 订阅**（不是按量付费），那标准的
`https://open.bigmodel.cn/api/paas/v4` 端点会返回
`code: 1113 余额不足或无可用资源包`——Coding Plan 流量被限制在专用端点：

```toml
[backend]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
model    = "GLM-4.5-air"   # 大写——这个端点对模型名大小写敏感
```

Coding Plan 下可用模型：`GLM-5.1` / `GLM-4.7` / `GLM-4.5-air` / `GLM-5-Turbo`。

## 接进你的客户端

`glance install` 自动做这些。如果想手动配置：

### codex CLI

`~/.codex/config.toml`：

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

或者手改 `~/.claude.json`：

```json
{
  "mcpServers": {
    "glance": { "type": "stdio", "command": "glance-mcp" }
  }
}
```

可选——在 `~/.claude/CLAUDE.md` 加一段路由提示，告诉模型"省 token 的活
优先用 glance"：

```markdown
- 读网页 → glance.web_fetch（不是 WebFetch）
- 搜网络 → glance.web_search（不是 WebSearch）
- 探 GitHub repo → glance.repo_explore（不要 clone+grep）
- 看图片 → glance.image_describe（不要让 Claude 直接看图）
- 跨文件调研 → glance.research（在改代码之前用）
```

### Cursor

`~/.cursor/mcp.json`：

```json
{ "mcpServers": { "glance": { "command": "glance-mcp" } } }
```

## 后端兼容性

任何说 **OpenAI 兼容** chat completions + function calling 的都行：

| 后端 | base_url | 备注 |
|---|---|---|
| **GLM（智谱）Coding Plan** | `https://open.bigmodel.cn/api/coding/paas/v4` | 模型名要大写 |
| **GLM（智谱）按量付费** | `https://open.bigmodel.cn/api/paas/v4` | 模型名小写 |
| **DeepSeek** | `https://api.deepseek.com` | 测试通过 |
| **OpenAI** | `https://api.openai.com/v1` | 直接当 upstream |
| **本地（Ollama / vLLM）** | `http://localhost:11434/v1` | 任何支持 function calling 的本地模型都行 |

挑便宜的就行。子代理循环不挑模型，只要支持 OpenAI function calling 就能跑。

## 内部怎么工作

1. codex / claude / cursor 通过 MCP stdio 协议调用比如
   `research(query="...", scope=[...])`
2. `glance-mcp` 接到调用，挑对应的工具 dispatcher
3. 该工具构造 system prompt，调 `sub_agent::run(system, user)`
4. 子代理循环驱动**你配置的后端**（GLM / DeepSeek / ...），带三个内置工具：
   `read_file`（大文件用 offset/limit 分页）/ `list_dir` / `grep`
5. 模型最多循环 `max_iterations` 次，按需调工具，直到返回一段最终文本
6. glance 把那段最终文本返回给 MCP 调用方——**永远不会把原始文件内容传出来**

大文件处理是关键：`read_file` 返回分页窗口（默认 400 行，最大 2000），
所以子代理可以先 `grep` 定位再读相关片段，不会被 100KB+ 的源文件呛到。

## GUI（可选）

`glance-app/` 是个 Tauri 2 菜单栏应用，**Claude Design** 编辑风格，
显示后端连通性 / 今日调用次数 / 字节节省 / 工具开关 / 实时
`events.jsonl` 日志 / Obsidian vault 选择器。装在 `/Applications/Glance.app`，
菜单栏图标点开就有。

build：

```bash
cd glance-app
npm install
npx tauri build --bundles app
# 产物：target/release/bundle/macos/Glance.app
```

## Chrome 桥（v0.3 新增）

`chrome` 工具让 Claude / Codex / Cursor 能**直接驱动你当前登录的 Chrome**——
复用所有 cookie / 已登录 session / 扩展 / 当前打开的标签页。和
`mcp__chrome-devtools__*` / `playwright` 这种"另开一个干净浏览器"不同，
这个走 Chrome 自己的 `chrome.scripting` + `chrome.debugger` API，**不需要**
你启动 `--remote-debugging-port`。

> **致敬 Codex**：架构思路是从 OpenAI Codex 桌面版的 `@chrome` plugin 学的
> ——一个 Chrome 扩展 + 原生消息 host + 内部协议。我们没复用任何 Codex 的
> 代码或扩展（Codex 的 `extension-host` 二进制锁了它自己的扩展 ID），但
> 三件套架构（扩展 ↔ native host ↔ MCP server）思路一致。**glance 这边
> 的实现完全独立，不依赖 Codex 跑没跑。**

### 它能做什么

| action | 用途 |
|---|---|
| `list_tabs` | 列出当前 Chrome 所有窗口的所有标签页（id / title / url / active） |
| `navigate` | 让某个 tab 跳到一个 URL |
| `wait_load` | 等 tab 进入 `complete` 状态（用于跳转后取数据） |
| `evaluate` | 在 tab 的 MAIN world 跑任意 JS，返回结果（带 `await_promise`） |
| `click` | querySelector + 派发 mousedown/up/click 事件，正确触发 SPA 框架 |
| `fill` | 给 input/textarea 设值并派发 input/change 事件 |
| `screenshot` | 截当前可见区，存成 PNG/JPEG（默认 `~/.glance/cache/`） |
| `cdp` | 透传任意 CDP 命令（`Page.captureScreenshot` / `Network.*` / `Runtime.*`） |

### 安装（4 步，3 分钟）

```bash
# 1. 拷扩展 + native host + 写 Chrome native messaging manifest
glance chrome install
```

这一步会：
- 把扩展放到 `~/.glance/chrome-bridge/extension/`（manifest 里硬编码了 RSA
  公钥，所以 Chrome 计算出的扩展 ID 是**确定的** `eofgbpadckhmkhhbbhekngmkgagfifhe`）
- 把 Node native host 放到 `~/.glance/chrome-bridge/host/`
- 在 `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/com.glance.chrome.json`
  写好 native host manifest，`allowed_origins` 已经指向上面那个固定 ID
- 自动帮你弹出 `chrome://extensions`

```
# 2. 在 chrome://extensions 里打开右上角"开发者模式"
# 3. 点"加载已解压的扩展"，选 ~/.glance/chrome-bridge/extension/
#    扩展 ID 应该自动是 eofgbpadckhmkhhbbhekngmkgagfifhe（manifest key 决定）
# 4. 在 Glance app 的 Tools 标签里把 chrome 开关打开（或 ~/.glance/config.toml 里 tools.chrome = true）
```

然后**重启 Claude Code / Codex / Cursor 会话**让它重新拉一次 `tools/list`。

`glance chrome status` 可以查当前绑定状态。

### 用例

```
"帮我看看现在打开的 OMS 销售页今天卖了什么"
→ chrome list_tabs → chrome evaluate (抓表格) → 总结

"把 LinuxDo 这条帖子内容贴给我"
→ chrome list_tabs → chrome evaluate (innerText) → 引用

"打开 GitHub 搜 zai codex，截图"
→ chrome navigate → chrome wait_load → chrome screenshot
```

### 为什么不用 chrome-devtools / playwright

可以用，差别在：
- **chrome-devtools MCP** 走 CDP 端口，需要你启 Chrome 时加 `--remote-debugging-port=9222`，且会暴露给本机所有进程
- **playwright** 起一个**全新的**干净浏览器，没有你的 cookie、扩展、登录态
- **glance.chrome** 用 Chrome 扩展接 `chrome.debugger` API，**用现有的 Chrome 进程**——开发者模式即可，不开调试端口

### 不让 Codex 看到这个工具

Codex 自带 `@chrome` plugin（同样原理），如果你两边都跑会撞车。
Glance app → Tools 标签 → 找到 `chrome` 行 → 点掉 `[codex]` pill 就行。

## Importing third-party adapters (v0.43)

[opencli](https://github.com/jackwener/opencli) 和它的 Rust 移植
[autocli](https://github.com/nashsu/autocli) 一共维护了 100+ 个站点 YAML 配
方（豆瓣、bilibili、Notion、Twitter、Boss 直聘……）。glance 没有把它们的
runtime 整套搬过来——那是几周的工作量，schema 还更复杂——而是写了一个**翻
译层**：

```
glance chrome import <yaml-file-or-dir>   # → ~/.glance/chrome-adapters/
glance chrome import examples/chrome-adapters/   # 试试仓库自带的 5 个
```

翻译器只接**浏览器 DOM 抓取**这一种 adapter（一个 `evaluate:` step，跑在
你已登录的 Chrome 页面里）。下面这些会被**跳过**并打印一行原因：

| 上游字段 | 处理 | 为什么 |
|---|---|---|
| `strategy: public` | skip | 是纯 HTTP fetch，没必要走浏览器——用 `glance.web_fetch` 或者直接 curl API 就行 |
| `strategy: intercept` / `auth: INTERCEPT` | skip | 需要拦网络包，glance.chrome 不做 |
| `auth: TOKEN` | skip | 需要在请求间追踪 token |
| pipeline 里有 `fetch:` / `collect:` / `intercept:` step | skip | 需要 opencli runtime |
| 多个 `evaluate:` step | skip | 我们一次只跑一段 JS |

剩下的会自动转换：`${{ args.foo }}` 占位符内联成 `args.foo`，整段 JS 包一
层让返回值统一成 `{ rows: [...] }`，根据 `endpoint` / `domain` /
`navigate:` 推一个 `match_url` 正则——之后 `chrome run_adapter {name, args}`
就能跑。

```bash
# 试一下（不写盘）
glance chrome import examples/chrome-adapters/ --dry-run

# 真写到 ~/.glance/chrome-adapters/
glance chrome import examples/chrome-adapters/

# 单文件
glance chrome import path/to/some-autocli.yaml --force
```

仓库 `examples/chrome-adapters/` 里默认带 5 个翻译干净的：
`douban_movie-hot` / `douban_book-hot` / `douban_top250` / `notion_sidebar` /
`bilibili_feed`。**不会**在安装时自动种到 `~/.glance/`——DOM 抓取的选择器
会随站点改版失效，强行预装等于给你埋雷。要用就 `import`，自己拥有。

> 何时该用 chrome adapter？只在站点**需要登录态**或**没有公开 API**的时候。
> 想抓公开网页的内容用 `glance.web_fetch` 更省 token、不用开 Chrome；想跑
> Hacker News / dev.to / arXiv 之类有公开 JSON / RSS API 的，直接 fetch
> 也行，不用走 chrome adapter。

属性归属：opencli / autocli 都是 Apache-2.0，每个 `examples/chrome-adapters/`
里的 YAML 顶部都注明了上游 URL + commit SHA + 原作者。翻译后的拷贝在
glance 仓库里继承 PolyForm-Noncommercial（仓库整体许可），upstream 的
Apache-2.0 NOTICE 在每个文件头部保留。

## 许可

[PolyForm Noncommercial 1.0.0](LICENSE) —— 个人 / 学习 / 业余项目 / 非营利
组织免费用。商用需另签许可，有意请开 issue。

## 致谢

- 架构（委派子代理省 token）灵感来自
  [CodexSaver](https://github.com/fendouai/CodexSaver)——同样的"委派"思路，
  不同的后端、不同的工具集、独立代码库。
- Chrome 桥（v0.3）的三件套架构（扩展 ↔ native host ↔ MCP server）参考自
  [OpenAI Codex 桌面版](https://openai.com/codex/)的 `@chrome` plugin
  设计；具体实现完全独立、不依赖 Codex。
- Chrome parity 工具（v0.41）surface 对照
  [Microsoft Playwright MCP](https://github.com/microsoft/playwright-mcp) 和
  [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp)，
  补齐了它们独有的 action 让 glance.chrome 是这两者的严格超集。
- v0.42 的 YAML 站点适配器思路来自
  [opencli](https://github.com/jackwener/opencli) /
  [autocli](https://github.com/nashsu/autocli)（100+ 站点配方）；schema 是
  glance 自己的最小子集，不直接复用他们的 runtime。
- v0.43 的 `glance chrome import` 翻译层把上游 Apache-2.0 的 YAML 转成
  glance schema（只接 DOM 抓取那一块），`examples/chrome-adapters/` 里 5 个
  样本都标了原始 URL + commit。
- 菜单栏 GUI 的 RTK 标签 + Token Killer 钩子集成自
  [rtk-ai/rtk](https://github.com/rtk-ai/rtk)——把 `git status` / `cargo test`
  这种高输出命令静默改写成 `rtk <cmd>`，AI agent 上下文里能省 60-90% 的 token。
- CCUSAGE 用量面板（每日 / 会话拆分）数据源自
  [ccusage](https://github.com/ryoppippi/ccusage)——把 Claude Code 的本地账
  单 sqlite 解析成可视化报表，glance 直接套了它的 CLI。
