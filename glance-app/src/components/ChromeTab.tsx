import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";

// Mirror of `glance::install::chrome::ChromeStatus` (commands.rs returns it
// via serde under camelCase by default, but Tauri preserves snake_case).
interface ChromeStatus {
  ext_dir: string;
  ext_dir_exists: boolean;
  host_dir: string;
  host_dir_exists: boolean;
  manifest_path: string;
  manifest_present: boolean;
  bound_extension_id?: string | null;
  expected_extension_id: string;
  socket_path: string;
  socket_present: boolean;
  bridge_connected: boolean;
  heartbeat_age_secs?: number | null;
  heartbeat_pid?: number | null;
}

interface InstallReport {
  ext_dir: string;
  host_dir: string;
  manifest_path: string;
  extension_id: string;
}

interface AdapterSummary {
  name: string;
  description: string | null;
  match_url: string | null;
  args: string[];
  source_path: string | null;
}

interface LastEvaluatePreview {
  tab_id: number;
  expression: string;
  await_promise: boolean;
  ts: number;
  tab_url: string;
}

interface Props {
  cfg: ReturnType<typeof import("../hooks/useConfig").useConfig>;
  active: NavEntry;
}

export function ChromeTab({ cfg, active }: Props) {
  const [status, setStatus] = useState<ChromeStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastInstall, setLastInstall] = useState<InstallReport | null>(null);
  const [adapters, setAdapters] = useState<AdapterSummary[]>([]);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<string>("");
  // "Save last evaluate as adapter" modal state.
  const [saveLastTabId, setSaveLastTabId] = useState<string>("");
  const [saveLastPreview, setSaveLastPreview] =
    useState<LastEvaluatePreview | null>(null);
  const [saveLastOpen, setSaveLastOpen] = useState(false);
  const [saveLastName, setSaveLastName] = useState("");
  const [saveLastDesc, setSaveLastDesc] = useState("");
  const [saveLastMatchUrl, setSaveLastMatchUrl] = useState("");

  async function refreshAdapters() {
    try {
      const list = await invoke<AdapterSummary[]>("chrome_adapter_list");
      setAdapters(list);
    } catch (e) {
      setError(String(e));
    }
  }
  useEffect(() => { refreshAdapters(); }, []);

  async function openAdapter(name: string) {
    try {
      const yaml = await invoke<string>("chrome_adapter_get", { name });
      setEditing(name);
      setDraft(yaml);
    } catch (e) {
      setError(String(e));
    }
  }

  async function saveAdapter() {
    if (!editing) return;
    setBusy("saving adapter");
    try {
      await invoke("chrome_adapter_save", { args: { name: editing, yaml: draft } });
      await refreshAdapters();
      setEditing(null);
      setDraft("");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function deleteAdapter(name: string) {
    if (!confirm(`删除适配器 "${name}" ？`)) return;
    setBusy("deleting adapter");
    try {
      await invoke("chrome_adapter_delete", { name });
      await refreshAdapters();
      if (editing === name) { setEditing(null); setDraft(""); }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function newAdapter() {
    const name = prompt("新适配器名（小写字母 / 数字 / 下划线 / 连字符）：");
    if (!name) return;
    const tpl =
`name: ${name}
description: |
  一行描述：这个适配器在哪个站点干什么。
match_url: '^https://example\\.com/'
args:
  - name: query
    required: false
    default: ''
await_promise: true
evaluate: |
  // \`args\` is the in-scope parameter object.
  // The last expression is what get returned to the LLM.
  ({ url: location.href, args })
`;
    setEditing(name);
    setDraft(tpl);
  }

  async function openSaveLastEvaluate() {
    setError(null);
    const raw = saveLastTabId.trim();
    if (!raw) {
      setError("先填 tab_id（用 chrome list_tabs 拿到的整数）");
      return;
    }
    const tabId = Number(raw);
    if (!Number.isInteger(tabId) || tabId <= 0) {
      setError(`无效的 tab_id: ${raw}`);
      return;
    }
    try {
      const preview = await invoke<LastEvaluatePreview | null>(
        "chrome_get_last_evaluate",
        { tabId },
      );
      if (!preview) {
        setError(
          `tab ${tabId} 还没有捕获到 evaluate — 先在该 tab 上跑一次 evaluate（且要返回非 null 值）。`,
        );
        return;
      }
      setSaveLastPreview(preview);
      setSaveLastName("");
      setSaveLastDesc("");
      setSaveLastMatchUrl("");
      setSaveLastOpen(true);
    } catch (e) {
      setError(String(e));
    }
  }

  function isValidAdapterName(s: string): boolean {
    return /^[a-z0-9_-]{1,32}$/.test(s);
  }

  async function commitSaveLastEvaluate() {
    if (!saveLastPreview) return;
    if (!isValidAdapterName(saveLastName)) {
      setError("名字需是 1-32 位 [a-z 0-9 _ -]");
      return;
    }
    setBusy("saving captured evaluate");
    setError(null);
    try {
      await invoke<string>("chrome_save_last_evaluate_as_adapter", {
        args: {
          tab_id: saveLastPreview.tab_id,
          name: saveLastName,
          description: saveLastDesc || null,
          match_url: saveLastMatchUrl || null,
        },
      });
      setSaveLastOpen(false);
      setSaveLastPreview(null);
      await refreshAdapters();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  // Poll status every 2s so the connection badge stays live.
  useEffect(() => {
    let alive = true;
    async function load() {
      try {
        const s = await invoke<ChromeStatus>("chrome_status");
        if (alive) setStatus(s);
      } catch (e) {
        if (alive) setError(String(e));
      }
    }
    load();
    const t = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  async function runInstall() {
    setBusy("installing");
    setError(null);
    try {
      const r = await invoke<InstallReport>("chrome_install");
      setLastInstall(r);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function runUninstall() {
    if (!confirm("移除扩展程序吗？这会删除 ~/.glance/chrome-bridge/ 和 native host manifest。Chrome 里加载的扩展需要你手动移除。")) return;
    setBusy("uninstalling");
    setError(null);
    try {
      await invoke("chrome_uninstall");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function openExtensionsPage() {
    try {
      await invoke("chrome_open_extensions_page");
    } catch (e) {
      setError(String(e));
    }
  }

  async function openExtensionDir() {
    try {
      await invoke("chrome_open_extension_dir");
    } catch (e) {
      setError(String(e));
    }
  }

  // Toggle the master `tools.chrome` boolean from this tab too — saves a click.
  function toggleChromeTool() {
    if (!cfg.cfg) return;
    const tools = cfg.cfg.tools as Record<string, boolean>;
    cfg.patch(
      (p) => ({ ...p, tools: { ...p.tools, chrome: !tools.chrome } }),
      { save: true },
    );
  }

  const installed = !!status?.ext_dir_exists && !!status?.host_dir_exists && !!status?.manifest_present;
  const idMismatch =
    status?.bound_extension_id != null &&
    status.bound_extension_id !== status.expected_extension_id;
  const toolEnabled = !!(cfg.cfg?.tools as any)?.chrome;
  const connected = !!status?.bridge_connected;

  const meta = (
    <>
      <span>
        STATUS{" "}
        <b className={connected ? "ink-good" : "ink-mute"}>
          {connected ? "已连接" : installed ? "未连接" : "未安装"}
        </b>
      </span>
      {connected && status?.heartbeat_age_secs != null && (
        <span>
          HEARTBEAT <b>{status.heartbeat_age_secs}s</b>
        </span>
      )}
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        {/* connection badge — Codex-style row */}
        <section className="chrome-banner">
          <div className="chrome-banner-icon">
            <svg width="36" height="36" viewBox="0 0 36 36" aria-hidden>
              <circle cx="18" cy="18" r="16" fill="#fff" />
              <circle cx="18" cy="18" r="6" fill="#1a73e8" />
              <path
                d="M18 2 a16 16 0 0 1 13.86 8 H18 a8 8 0 0 0 -7.42 5 z"
                fill="#ea4335"
              />
              <path
                d="M31.86 10 A16 16 0 0 1 24 32.36 L17.34 23 a8 8 0 0 0 7.45 -5 z"
                fill="#fbbc05"
              />
              <path
                d="M24 32.36 A16 16 0 0 1 4.14 10 H17.34 a8 8 0 0 0 0 16 z"
                fill="#34a853"
              />
            </svg>
          </div>
          <div className="chrome-banner-text">
            <div className="chrome-banner-title">Google Chrome</div>
            <div className={`chrome-banner-sub ${connected ? "ok" : "muted"}`}>
              {connected
                ? "已连接到浏览器扩展程序，Glance 可驱动你的 Chrome"
                : installed
                  ? "扩展未连接 — 还没在 chrome://extensions 里加载，或扩展已被禁用"
                  : `尚未安装 — 点下方"安装"开始`}
            </div>
          </div>
          <label className="toggle teal chrome-banner-toggle" title={toolEnabled ? "关闭 chrome 工具（其他客户端也看不到）" : "开启 chrome 工具"}>
            <input type="checkbox" checked={toolEnabled} onChange={toggleChromeTool} />
            <span className="toggle-track" />
          </label>
        </section>

        {error && (
          <div className="chrome-error">
            <b>error:</b> {error}
          </div>
        )}

        {/* setup wizard */}
        <section className="chrome-section">
          <header className="chrome-section-header">
            <span className="chrome-section-title">安装步骤</span>
            <span className="chrome-section-sub">
              Chrome 不允许程序化加载未打包扩展，第 2 步必须你来。
            </span>
          </header>

          <ol className="chrome-steps">
            <li className={installed ? "step done" : "step active"}>
              <div className="step-num">1</div>
              <div className="step-body">
                <div className="step-title">
                  拷扩展 + native host + 写 NativeMessagingHosts manifest
                </div>
                <div className="step-desc">
                  落到 <code>~/.glance/chrome-bridge/</code>，并在 Chrome 的
                  manifest 目录写 <code>com.glance.chrome.json</code>，自动
                  绑定到固定扩展 ID{" "}
                  <code>{status?.expected_extension_id ?? "..."}</code>。
                </div>
                <div className="step-actions">
                  <button
                    className="btn primary"
                    disabled={busy != null}
                    onClick={runInstall}
                  >
                    {installed ? "重新安装扩展程序" : "安装扩展程序"}
                  </button>
                  {installed && (
                    <button
                      className="btn danger"
                      disabled={busy != null}
                      onClick={runUninstall}
                    >
                      移除扩展程序
                    </button>
                  )}
                </div>
              </div>
            </li>

            <li className={connected ? "step done" : installed ? "step active" : "step pending"}>
              <div className="step-num">2</div>
              <div className="step-body">
                <div className="step-title">
                  在 Chrome 里加载已解压的扩展
                </div>
                <div className="step-desc">
                  打开 <code>chrome://extensions</code> → 右上角"开发者模式"开 →
                  "加载已解压的扩展" → 选目录：
                </div>
                <div className="step-path">
                  <code>{status?.ext_dir ?? "~/.glance/chrome-bridge/extension"}</code>
                </div>
                <div className="step-actions">
                  <button
                    className="btn"
                    disabled={!installed}
                    onClick={openExtensionsPage}
                  >
                    打开 chrome://extensions
                  </button>
                  <button
                    className="btn"
                    disabled={!installed}
                    onClick={openExtensionDir}
                  >
                    在 Finder 中显示
                  </button>
                </div>
              </div>
            </li>

            <li className={toolEnabled ? "step done" : connected ? "step active" : "step pending"}>
              <div className="step-num">3</div>
              <div className="step-body">
                <div className="step-title">在 Glance 启用 chrome 工具</div>
                <div className="step-desc">
                  master 开关在右上角的 banner 里 / 或者去 Tools 标签调。
                  开了之后<b>重启你的 MCP 客户端</b>（Claude Code / Codex / Cursor）
                  让它重新拉一次 <code>tools/list</code>，新工具就出现了。
                </div>
              </div>
            </li>
          </ol>
        </section>

        {/* details / debug info */}
        <section className="chrome-section">
          <header className="chrome-section-header">
            <span className="chrome-section-title">运行时信息</span>
          </header>
          <dl className="chrome-info">
            <dt>扩展目录</dt>
            <dd>
              <code>{status?.ext_dir}</code>{" "}
              <span className={status?.ext_dir_exists ? "ok" : "bad"}>
                {status?.ext_dir_exists ? "✓" : "✗"}
              </span>
            </dd>
            <dt>Native host</dt>
            <dd>
              <code>{status?.host_dir}</code>{" "}
              <span className={status?.host_dir_exists ? "ok" : "bad"}>
                {status?.host_dir_exists ? "✓" : "✗"}
              </span>
            </dd>
            <dt>Chrome manifest</dt>
            <dd>
              <code>{status?.manifest_path}</code>{" "}
              <span className={status?.manifest_present ? "ok" : "bad"}>
                {status?.manifest_present ? "✓" : "✗"}
              </span>
            </dd>
            <dt>扩展 ID</dt>
            <dd>
              <code>{status?.bound_extension_id ?? "—"}</code>
              {idMismatch && (
                <span className="bad" style={{ marginLeft: 8 }}>
                  ⚠ 与预期不符（应为 <code>{status?.expected_extension_id}</code>）
                </span>
              )}
            </dd>
            <dt>Unix socket</dt>
            <dd>
              <code>{status?.socket_path}</code>{" "}
              <span className={status?.socket_present ? "ok" : "bad"}>
                {status?.socket_present ? "✓" : "✗"}
              </span>
            </dd>
            <dt>Heartbeat</dt>
            <dd>
              {status?.heartbeat_age_secs != null ? (
                <>
                  <code>
                    {status.heartbeat_age_secs}s 前（host pid={" "}
                    {status.heartbeat_pid ?? "?"}）
                  </code>{" "}
                  <span className={connected ? "ok" : "bad"}>
                    {connected ? "✓ live" : "✗ stale"}
                  </span>
                </>
              ) : (
                <span className="bad">无 heartbeat — 扩展未连上 native host</span>
              )}
            </dd>
          </dl>
        </section>

        {/* adapters — v0.42 */}
        <section className="chrome-section">
          <header className="chrome-section-header">
            <span className="chrome-section-title">站点适配器</span>
            <span className="chrome-section-sub">
              YAML 配方让 LLM 跳过现写 evaluate，直接调既定查询。
              <code>~/.glance/chrome-adapters/*.yaml</code>
            </span>
          </header>
          <div className="adapter-actions-row" style={{ flexWrap: "wrap", gap: 8 }}>
            <button className="btn primary" onClick={newAdapter}>+ 新适配器</button>
            <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
              <input
                type="text"
                inputMode="numeric"
                placeholder="tab_id"
                value={saveLastTabId}
                onChange={(e) => setSaveLastTabId(e.target.value)}
                style={{ width: 90, fontFamily: "monospace" }}
                title="目标 tab 的整数 id（用 chrome list_tabs 拿）"
              />
              <button
                className="btn"
                disabled={!saveLastTabId.trim()}
                onClick={openSaveLastEvaluate}
                title="把该 tab 上最近一次成功的 evaluate 保存为适配器"
              >
                保存最近 evaluate 为适配器
              </button>
            </span>
            <button className="btn" onClick={() => invoke("chrome_adapter_open_dir")}>
              在 Finder 中显示
            </button>
            <button className="btn" onClick={refreshAdapters}>刷新</button>
          </div>
          {adapters.length === 0 ? (
            <div className="adapter-empty">
              暂无适配器。点 <b>+ 新适配器</b> 写第一条，或者让 LLM 跑一次 evaluate
              后把脚本保存进来。
            </div>
          ) : (
            <table className="adapter-table">
              <thead>
                <tr>
                  <th>name</th>
                  <th>description</th>
                  <th>match_url</th>
                  <th>args</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {adapters.map((a) => (
                  <tr key={a.name}>
                    <td><code>{a.name}</code></td>
                    <td className="muted">{a.description || ""}</td>
                    <td><code className="match-url">{a.match_url || "—"}</code></td>
                    <td>{a.args.length ? a.args.join(", ") : "—"}</td>
                    <td className="adapter-row-actions">
                      <button className="btn" onClick={() => openAdapter(a.name)}>编辑</button>
                      <button className="btn danger" onClick={() => deleteAdapter(a.name)}>删</button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {editing && (
            <div className="adapter-editor">
              <div className="adapter-editor-head">
                <span>编辑：<code>{editing}</code></span>
                <span className="muted" style={{ fontSize: 11 }}>
                  保存前会做 YAML 解析校验。<code>evaluate</code> 体里 <code>args</code> 已注入为常量。
                </span>
              </div>
              <textarea
                className="adapter-editor-text"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                spellCheck={false}
              />
              <div className="adapter-editor-actions">
                <button className="btn primary" disabled={busy != null} onClick={saveAdapter}>
                  保存
                </button>
                <button className="btn" disabled={busy != null} onClick={() => { setEditing(null); setDraft(""); }}>
                  取消
                </button>
              </div>
            </div>
          )}
          {saveLastOpen && saveLastPreview && (
            <div
              className="adapter-editor"
              style={{
                marginTop: 12,
                border: "1px solid var(--ink-line, #ccc)",
                padding: 12,
                borderRadius: 6,
                background: "var(--ink-bg-soft, #fafafa)",
              }}
            >
              <div className="adapter-editor-head" style={{ marginBottom: 8 }}>
                <span>保存最近 evaluate 为适配器</span>
                <span className="muted" style={{ fontSize: 11 }}>
                  tab <code>{saveLastPreview.tab_id}</code> · captured ts={saveLastPreview.ts}
                </span>
              </div>
              <div style={{ fontSize: 12, marginBottom: 6 }} className="muted">
                来源 URL：<code>{saveLastPreview.tab_url || "—"}</code>
              </div>
              <pre
                style={{
                  maxHeight: 160,
                  overflow: "auto",
                  background: "var(--ink-bg, #fff)",
                  border: "1px solid var(--ink-line, #ddd)",
                  padding: 8,
                  fontSize: 11,
                  margin: 0,
                  whiteSpace: "pre-wrap",
                }}
              >
                {saveLastPreview.expression.length > 4000
                  ? saveLastPreview.expression.slice(0, 4000) + "\n…(truncated for preview)"
                  : saveLastPreview.expression}
              </pre>
              <div style={{ display: "grid", gap: 6, marginTop: 10 }}>
                <label style={{ display: "grid", gap: 2 }}>
                  <span style={{ fontSize: 11 }}>name (必填，1-32 位 [a-z 0-9 _ -])</span>
                  <input
                    type="text"
                    value={saveLastName}
                    onChange={(e) => setSaveLastName(e.target.value)}
                    placeholder="my_adapter"
                    style={{ fontFamily: "monospace" }}
                  />
                  {saveLastName && !isValidAdapterName(saveLastName) && (
                    <span style={{ color: "crimson", fontSize: 11 }}>
                      不合法：仅允许 a-z 0-9 _ -，最长 32
                    </span>
                  )}
                </label>
                <label style={{ display: "grid", gap: 2 }}>
                  <span style={{ fontSize: 11 }}>description (可选)</span>
                  <input
                    type="text"
                    value={saveLastDesc}
                    onChange={(e) => setSaveLastDesc(e.target.value)}
                    placeholder="一行说明这个 adapter 干什么"
                  />
                </label>
                <label style={{ display: "grid", gap: 2 }}>
                  <span style={{ fontSize: 11 }}>
                    match_url (可选；留空则从 tab URL 自动派生 ^scheme://host/)
                  </span>
                  <input
                    type="text"
                    value={saveLastMatchUrl}
                    onChange={(e) => setSaveLastMatchUrl(e.target.value)}
                    placeholder="^https://example\\.com/"
                    style={{ fontFamily: "monospace" }}
                  />
                </label>
              </div>
              <div className="adapter-editor-actions" style={{ marginTop: 10 }}>
                <button
                  className="btn primary"
                  disabled={busy != null || !isValidAdapterName(saveLastName)}
                  onClick={commitSaveLastEvaluate}
                >
                  保存
                </button>
                <button
                  className="btn"
                  disabled={busy != null}
                  onClick={() => {
                    setSaveLastOpen(false);
                    setSaveLastPreview(null);
                  }}
                >
                  取消
                </button>
              </div>
            </div>
          )}
        </section>

        {lastInstall && (
          <section className="chrome-section">
            <header className="chrome-section-header">
              <span className="chrome-section-title">上次安装</span>
            </header>
            <div className="chrome-install-summary">
              ✓ 写入了：
              <ul>
                <li><code>{lastInstall.ext_dir}</code></li>
                <li><code>{lastInstall.host_dir}</code></li>
                <li><code>{lastInstall.manifest_path}</code></li>
              </ul>
              扩展 ID（确定性）：<code>{lastInstall.extension_id}</code>
            </div>
          </section>
        )}
      </div>
    </div>
  );
}
