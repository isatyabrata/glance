# Glance 开发日志：v0.41 → v0.60

一周内 20 个版本。整理从「Chrome 桥起步」一路打到「能用自己的桥后台静默发完
22 条推 + 1 篇 Article」的全过程。

---

## 第一阶段（v0.41–v0.46）Chrome 桥 + 站点适配器

把 chrome MCP 从能用做到了好用。

- **v0.41** Chrome 桥 surface 对齐——把 `@playwright/mcp` 和 `chrome-devtools-mcp`
  全跑了一遍，独有 action 全补齐。glance.chrome 成为这两者超集。28 个 action：
  navigate / click / wait_for / press_key / hover / drag / network / console /
  dialog / emulate / heap_snapshot / start_trace ...
- **v0.42** YAML 站点适配器：`~/.glance/chrome-adapters/<name>.yaml` 把高频查询
  固化，`run_adapter` 一击拿结构化 JSON。GUI 内置 YAML 编辑器，保存前 parse 校验。
- **v0.43** Native messaging host 用 Rust 重写。删掉 Node 依赖，`cargo install`
  直接铺 `glance-chrome-host` 到 PATH。
- **v0.44** `lighthouse_audit` action + 自动捕获（菜单栏 app 里"刚跑的 evaluate
  存为适配器"按钮）。
- **v0.45** `glance chrome import`——把 [opencli](https://github.com/jackwener/opencli)
  / [autocli](https://github.com/nashsu/autocli) 的站点 YAML 翻译成 glance schema。
  schema 自己一套，不实现他们的 runtime。
- **v0.46** Vendor 65 条 autocli 适配器到 `examples/chrome-adapters/`，
  Apache-2.0 attribution 完整保留。覆盖 27 个站点：豆瓣 / B 站 / Reddit / 雪球 /
  Twitter / TikTok / Instagram / Facebook / Notion / Medium / linux.do / 知乎 /
  微博 / 即刻 / Boss / barchart / 小红书 / v2ex / Substack / 新浪博客 / 路透 /
  携程 / 超星 / Bloomberg ... 70+ 条一行 `glance chrome import` 全装。

---

## 第二阶段（v0.47–v0.55）X 自动发推 — 9 道防线一道道破

想用 glance 自己的 chrome bridge 发关于 glance 的 thread。结果 X 把整个反自动化
仓库都掏出来了：CSP、isTrusted、Draft.js 多块编辑器、focus 检查、modal 懒挂载——
每一道防线都得单独破。

### 第 1 道墙：CSP `unsafe-eval`

X 严格 CSP 禁所有 `new Function(code)`。我们的 `evaluate` 走
`chrome.scripting.executeScript` + `new Function`，在 X 上 100% 报错。

**v0.47 fix**：`evaluate` 加 `world: "cdp"`，走 `chrome.debugger.Runtime.evaluate`。
debugger 特权绕开页面 CSP，evaluate 在 X 上能跑了。

### 第 2 道墙：Draft.js 拒接合成 ClipboardEvent

`new ClipboardEvent("paste", {clipboardData: dt})` + `dispatchEvent` 看起来很美，
Draft.js 调用 `preventDefault`，但 `isTrusted: false` 静默丢弃 data。文字不进 editor。

**v0.49 fix**：`os_paste` action —— Rust 端 `pbcopy` 把文字塞系统剪贴板，
extension 端 CDP `Input.dispatchKeyEvent` 带 `commands: ["paste"]`。
OS 级真粘贴，`isTrusted: true`，Draft.js 接。

### 第 3 道墙：OS 剪贴板需要 Chrome 在前台

`commands: ["paste"]` 读系统剪贴板时如果 Chrome 不是顶层窗口（用户在 Claude Code
窗口），macOS 不让访问。`document.hasFocus() === false` 也间接锁定。

**v0.54 fix**：`type_multiline` action —— CDP `Input.insertText` 整段塞 +
段间 CDP Enter 键 + 600ms React 提交 settle。完全不碰剪贴板。
CDP 路由不需要 OS focus，每个 keystroke 都 `isTrusted: true`。
跑得比 os_paste 慢约 1 秒，但 Chrome 后台时唯一能用的路径。

### 第 4 道墙：合成 click 不弹 modal

SideNav Post 按钮的 onClick 检查 `event.isTrusted`。
`btn.click()` 或 `dispatchEvent(MouseEvent)` 触发不了 modal 渲染——
URL 改成 `/compose/post` 但 modal shell 是空的。

**v0.53 fix**：`click_native` 走 CDP `Input.dispatchMouseEvent`（mousePressed +
mouseReleased）at viewport coords。OS 级 trusted click，X 的 onClick 看不出
和真鼠标的区别。

### 第 5 道墙：modal 内容懒挂载查 `document.hasFocus()`

最隐蔽的一道。SideNav click 后 URL 变 `/compose/post`，`<div role="dialog">`
壳子也挂载了，但里面的 `data-viewportview="true"` 容器是**空的**——没 editor、
没 file input、没 Post 按钮。

X compose modal 内部用 IntersectionObserver / focus 事件触发"挂载实际表单"。
Chrome 在后台时 `document.hasFocus() === false`，X 认为 tab 是 background
跳过 lazy mount。

**找到关键 CDP API**：`Emulation.setFocusEmulationEnabled` —— 让页面以为有焦点
但不真夺 OS focus。这是测试框架的标准做法。

```javascript
await chrome.debugger.sendCommand({tabId}, "Emulation.setFocusEmulationEnabled",
  {enabled: true});
// 立刻 document.hasFocus() = true，但 Chrome 还在后台
```

加上这个 modal 内容立刻挂载。这是整个发推流程最关键的一发现。

### 第 6 道墙：Cmd+Enter 提交也查 isTrusted

合成 click 提交按钮再次被 X 静默拒。

**v0.51 fix**：`submit_post` action —— focus editor + CDP Cmd+Enter。
比 click 按钮更"用户化"，X / Bluesky / Slack 几乎所有现代富文本编辑器都识
Cmd+Enter / Ctrl+Enter 作为 send 快捷键。

### v0.55 — 烧死一个 action

把整套流程烧进 Rust 内置 `chrome.tweet` action：

```
Emulation.setFocusEmulationEnabled  ← 焦点模拟
click SideNav Post                   ← trusted CDP click 弹 modal
wait modal mount
type_multiline 文字                  ← 不需要剪贴板，多段安全
upload_file 图片                     ← CDP DOM.setFileInputFiles
4s review delay                     ← 看起来像人在 review
submit_post (Cmd+Enter)             ← OS 级 trusted 提交
wait 5.5s commit
读 latest /status/ URL
```

LLM 一行 `chrome { action: "tweet", value, files? }` 一条推。本仓库今天的
22 条 thread + 1 篇 Article 全是这个 action 发的，全程 Chrome 后台、用户继续
在 Claude Code 里干活，X 没拦截、没限流。

### 第 7 道墙（彩蛋）：UX 反馈

CDP 操作时 Chrome 显示"Glance Chrome Bridge started debugging this browser"
黄条，用户看着不爽。

**v0.50** 加 chrome.debugger 60s 闲置自动 detach——黄条不再持续显示，只在真
正用 CDP 那一刻弹出一次。
**v0.52** 加视觉鼠标 SVG overlay——`click_native` / `hover` 前先在目标位置
画一个紫色光标，250ms settle 让用户看见再 dispatch 真事件。Codex `@chrome`
插件同款。

---

## 第三阶段（v0.56–v0.60）可靠性 + 可观测

通过 X 这一战发现的踩坑点全部固化成超时闸门和可见指标。

### v0.56 sub_agent 全双超时

`research` / `explain` / `search` 在 GLM 后端慢时撞 MCP 客户端的 120s 硬墙——
主模型只能 fallback 自己干，"省 token"反被打脸。

加两层：
- `deadline_secs` (90s)：sub_agent 整个 run 的 wall-clock 总闸
- `chat_timeout_secs` (45s after v0.60)：单次 GLM 调用超时

到点返回带 `[glance partial: <why> after N iters / M tool calls — advice]`
后缀的部分摘要。主模型立刻知道：要么 narrow scope 要么 fallback to local
Grep / Read。**永远不会让 MCP 客户端等满 120s**。

### v0.57 transport 110s 总闸 + 上游 100s

mysql-prints10-oms 大聚合查询撞 60s 上游 timeout。两个改：
- aggregator 上游 timeout 60→100s（给真长查询余量）
- transport 加 110s wall-clock 总闸（任何路径都不会让 Claude Code 等满 120s）

110/100 分层：上游 100s 内出结果或报错，再留 10s 给 transport 收尾。
两者都在 MCP 客户端的 120s 之内。

### v0.58 缓存命中率可见化

读了 [@shachepi 的提示词缓存文章](https://x.com/shachepi/status/2053463461729046817)，
对照 glance：
- ChatRequest 字段顺序改 `tools → messages` 对齐 Anthropic 前缀缓存
- `Usage` 用 custom Deserialize 归一化解析三种缓存字段：
  - OpenAI `prompt_tokens_details.cached_tokens`
  - Anthropic `cache_read_input_tokens`
  - DeepSeek `prompt_cache_hit_tokens`
- 通过 CallCtx 累加 → ToolEvent → events.jsonl 持久化
- Tauri menubar Usage 面板可直接看 `cache_hit_rate = glm_cached_tokens / glm_prompt_tokens`

**"看不见就改不动"**——这步走完之前我们的 sub_agent 缓存命中率根本是黑盒。

### v0.59 evaluate 封装三修

观察到另一个 agent 撞 `evaluate` 返回空——一路 fallback 到 `console.log +
list_console_messages` 才拿到数据。挖出三个 bug：

1. **IIFE 双 wrap**：单表达式 `document.body` 我们 wrap 成
   `(async () => { return (document.body); })()`，但如果传 IIFE 进来再 wrap，
   生成的 `(async () => { return ((async () => {...})()); })()` 在某些
   `const`/`let` 场景下语法错。Fix：检测表达式起手 `(` / `async ` 就跳过 wrap。

2. **不可序列化返回静默 null**：DOM 节点 / Function / Promise / 循环对象
   `returnByValue` 失败时 `r.value === undefined`。我们之前返回 null，
   agent 没线索。Fix：返回 `{__glance_eval__: "non-serializable", type,
   subtype, className, hint}` 诊断对象。

3. **没有 raw 模式**：多语句脚本（如 `const x = ...; console.log(x); x.foo`）
   不能用 IIFE 自然包。Fix：加 `raw: true` 参数完全跳过 wrap。

### v0.60 sub_agent 0-iter partial 文案

观察到 chat_timeout 25s 经常吃掉第一次冷调用（GLM TLS handshake + model warmup
能花 30-40s）。0 iter partial 配的"narrow the scope"建议是错的——agent 该
fallback 到本地 Grep / Read 而不是再 narrow。

两修：
- 默认 chat_timeout 25→45s（给冷启动余量）
- finalize_partial 根据 iter count 给不同建议：
  - `iter == 0` → "GLM backend slow / unreachable — fall back to local Grep / Read"
  - `iter > 0` → "narrow the scope or split the query"

---

## 一些跨版本的洞察

### 路由偏置：让 Claude 只看见我们的 chrome 工具

Chrome 自动化场景 Claude 同时看见 `mcp__glance__chrome` + `mcp__glance__playwright__*`
(24 个) + `mcp__glance__chrome-devtools__*` (29 个)。Claude 不可能每次都选对。
三层叠加偏置（commit `ccb6edf`）：

1. **`~/.glance/config.toml`**：`upstream_mcps.playwright.clients = ["codex"]`
   + 同对 chrome-devtools。Claude / Cursor 完全看不到这俩 upstream。
2. **`~/.claude/CLAUDE.md`** 加表格行 "Drive Chrome → use `mcp__glance__chrome`,
   NOT chrome-devtools / playwright"。
3. **`src/tools/chrome.rs`** description 起手 "PREFERRED OVER chrome-devtools__\*
   and playwright__\*" + 3 条理由。

效果：Claude 那边的 `tools/list` 从 52 个工具减到 43 个，混淆 0，规则强制。
Codex 那边照旧能用 chrome-devtools / playwright（场景：sandboxed 浏览器测试）。

### Token 经济的四个角（v0.46 前已成型，这次系统总结）

- **委托子代理**（research / explain）：AI 思考省，主模型不必读 8KB 原始代码
- **网关工具**（web_fetch / repo_explore / image_describe / web_search）：
  Anthropic 配额省，路由到 GLM
- **rtk**（Rust Token Killer）：Bash 输出省，git push / cargo test 千行进、
  8% 行出
- **ccusage**：看见省了多少，按日 / session 拆 input/output/cache token

v0.58 加了第五条腿：**缓存命中率**指标，让前四个的效果更精确。

---

## 数据回顾

- **20 个版本** v0.41 → v0.60 一周内合并到 main
- **35 个 chrome action**（v0.41 是 28，加了 7 个：set_contenteditable / paste_text /
  os_paste / type_multiline / submit_post / tweet + 内部辅助）
- **22 条推文 + 1 篇 Article** 全部由 glance 自己的 chrome 桥后台静默发出
- **9 道 X 反自动化防线**全部破解、固化进 `chrome.tweet` 一行 action

---

## 仓库

[github.com/isatyabrata/glance](https://github.com/isatyabrata/glance)

PolyForm Noncommercial 1.0。安装：

```bash
cargo install --git https://github.com/isatyabrata/glance
glance install
glance chrome install
```
