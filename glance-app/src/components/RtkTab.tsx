import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavEntry } from "../App";
import type {
  RtkClient,
  RtkGain,
  RtkHistoryEntry,
  RtkStatus,
  RtkUpdateCheck,
  RtkUpdateResult,
} from "../types";
import { TabHeader } from "./TabHeader";

interface Props {
  active: NavEntry;
}

const HISTORY_LIMIT = 30;
const REFRESH_MS = 5000;

function fmtBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}K`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(2)}M`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)}G`;
}

function fmtBytesParts(n: number): { num: string; unit: string } {
  if (n < 1024) return { num: `${n}`, unit: "B" };
  if (n < 1024 * 1024) return { num: (n / 1024).toFixed(1), unit: "KB" };
  if (n < 1024 * 1024 * 1024) return { num: (n / 1024 / 1024).toFixed(2), unit: "MB" };
  return { num: (n / 1024 / 1024 / 1024).toFixed(2), unit: "GB" };
}

function nf(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

function shortTime(ts: string): string {
  // Render as local HH:MM:SS so the table matches the user's wall clock.
  // rtk's history rows arrive as either RFC 3339 UTC or unix epoch — both
  // parse via Date(). If neither (already a bare HH:MM:SS), keep as-is.
  const d = new Date(ts);
  if (!isNaN(d.getTime())) return d.toLocaleTimeString("zh-CN", { hour12: false });
  const m = ts.match(/(\d{2}:\d{2}:\d{2})/);
  return m ? m[1] : ts;
}

function truncCommand(cmd: string, max = 40): string {
  // Strip the leading "rtk " prefix so the column shows the real underlying
  // command (rtk rewrites every entry to start with `rtk`, and that prefix
  // is dead weight when scanning a list of identical-shaped rows).
  let c = cmd;
  if (c.startsWith("rtk ")) c = c.slice(4);
  if (c.length <= max) return c;
  return c.slice(0, max - 1) + "…";
}

const CLIENTS: Array<{ id: RtkClient; label: string; sub: string }> = [
  { id: "claude",  label: "Claude Code", sub: "hook · settings.json" },
  { id: "codex",   label: "Codex CLI",   sub: "AGENTS.md · @RTK.md" },
  { id: "cursor",  label: "Cursor",      sub: "hook · ~/.cursor" },
];

export function RtkTab({ active }: Props) {
  const [status, setStatus] = useState<RtkStatus | null>(null);
  const [gain, setGain] = useState<RtkGain | null>(null);
  const [history, setHistory] = useState<RtkHistoryEntry[]>([]);
  const [update, setUpdate] = useState<RtkUpdateCheck | null>(null);
  const [topErr, setTopErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null); // e.g. "install:claude"
  const [actionLog, setActionLog] = useState<string | null>(null);
  const [visible, setVisible] = useState<boolean>(!document.hidden);

  const refresh = useCallback(async () => {
    try {
      const [s, g, h] = await Promise.all([
        invoke<RtkStatus>("rtk_status"),
        invoke<RtkGain>("rtk_gain").catch(() => null),
        invoke<RtkHistoryEntry[]>("rtk_history", { limit: HISTORY_LIMIT }).catch(
          () => [] as RtkHistoryEntry[],
        ),
      ]);
      setStatus(s);
      if (g) setGain(g);
      setHistory(h);
      setTopErr(null);
    } catch (e) {
      setTopErr(`${e}`);
    }
  }, []);

  // Disable polling when the tab/window is not visible — the visibility API
  // covers system sleep + cmd-tab away. The tab being mounted but the
  // sidebar pointing elsewhere is implicitly handled because App.tsx
  // unmounts inactive tabs.
  useEffect(() => {
    const onVis = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, []);

  useEffect(() => {
    refresh();
    if (!visible) return;
    const t = window.setInterval(refresh, REFRESH_MS);
    return () => window.clearInterval(t);
  }, [refresh, visible]);

  // Check for new release once on mount + every time the tab is reopened.
  // GitHub allows 60 anonymous req/h — at most a couple hits per session.
  useEffect(() => {
    if (!visible) return;
    invoke<RtkUpdateCheck>("rtk_check_update")
      .then(setUpdate)
      .catch(() => {});
  }, [visible]);

  async function runUpdate() {
    setBusy("update");
    setActionLog("running brew upgrade rtk … (or cargo install fallback)");
    try {
      const r = await invoke<RtkUpdateResult>("rtk_update");
      const head = r.stdout.split("\n").slice(0, 3).join(" · ");
      setActionLog(
        r.ok
          ? `✓ updated via ${r.method} · ${head}`
          : `✗ update via ${r.method} failed · ${r.stderr.slice(0, 200)}`,
      );
      await refresh();
      try {
        const u = await invoke<RtkUpdateCheck>("rtk_check_update");
        setUpdate(u);
      } catch {}
    } finally {
      setBusy(null);
    }
  }

  async function runAction(kind: "install" | "uninstall", client: RtkClient) {
    const key = `${kind}:${client}`;
    setBusy(key);
    setActionLog(null);
    try {
      const cmd = kind === "install" ? "rtk_init" : "rtk_uninstall";
      const out = await invoke<string>(cmd, { client });
      setActionLog(out || `${kind} ${client} done`);
      await refresh();
    } catch (e) {
      setActionLog(`✗ ${e}`);
    } finally {
      setBusy(null);
    }
  }

  const headerPill = useMemo(() => {
    if (!status) return { text: "checking", tone: "idle" as const };
    if (!status.installed) return { text: "not installed", tone: "err" as const };
    if (!status.claude_hook && !status.codex_agents_md && !status.cursor_hook) {
      return { text: "no hooks", tone: "idle" as const };
    }
    return { text: `v${status.version || "?"}`, tone: "ok" as const };
  }, [status]);

  const overallDot: "ok" | "err" | "idle" | "warm" = !status
    ? "idle"
    : !status.installed
    ? "err"
    : status.claude_hook
    ? "ok"
    : "warm";

  const meta = (
    <>
      <span>
        CALLS <b>{nf(gain?.total_commands ?? 0)}</b>
      </span>
      <span>
        SAVED <b>{fmtBytes(gain?.total_saved ?? 0)}</b>
      </span>
    </>
  );

  const savedFmt = fmtBytesParts(gain?.total_saved ?? 0);

  function clientHookOn(c: RtkClient): boolean {
    if (!status) return false;
    if (c === "claude") return status.claude_hook;
    if (c === "codex") return status.codex_agents_md;
    return status.cursor_hook;
  }

  return (
    <div className="tab-page">
      <TabHeader active={active} pill={headerPill} meta={meta} />

      <div className="tab-body">
        {topErr && <div className="error-line">{topErr}</div>}

        {/* ── 1. Status row ───────────────────────────────────────── */}
        <div className="upstream-card" style={{ borderTop: "1px solid var(--rule)" }}>
          <div className="upstream-card-head">
            <span className={`dot-led ${overallDot}`} />
            <span className="upstream-card-name">rtk</span>
            <span className="upstream-card-kind">
              {status?.version ? `v${status.version}` : "—"}
            </span>
            <span className="upstream-card-status">
              {!status
                ? "checking…"
                : status.installed
                ? "installed"
                : "not on PATH"}
            </span>
            <span className="upstream-card-tools">
              {gain?.total_commands ?? 0} rewrites
            </span>
          </div>
          <div className="upstream-card-detail" title={status?.binary_path ?? ""}>
            {status?.binary_path ?? "binary not found — `brew install rtk`"}
          </div>
          {!status?.installed && (
            <div className="upstream-card-err">
              ⚠ rtk binary missing. Install it first:{" "}
              <code>brew install rtk</code> (rtk-ai/rtk on GitHub).
            </div>
          )}
          {/* Update badge — shows current → latest when GitHub release is newer */}
          {update && (
            <div className="rtk-update-row">
              <span className="smallcaps-tiny">version</span>
              {update.outdated ? (
                <>
                  <span className="rtk-update-badge warn">
                    {update.current ?? "?"} → {update.latest ?? "?"} update available
                  </span>
                  <button
                    type="button"
                    className="ping-btn"
                    onClick={runUpdate}
                    disabled={busy === "update"}
                  >
                    {busy === "update" ? "updating…" : "update"}
                  </button>
                </>
              ) : update.current && update.latest ? (
                <span className="muted">
                  {update.current} (latest {update.latest})
                </span>
              ) : (
                <span className="muted">checking GitHub releases…</span>
              )}
            </div>
          )}
        </div>

        {/* ── 2. Per-client install matrix ───────────────────────── */}
        <div className="rtk-clients">
          {CLIENTS.map((c) => {
            const on = clientHookOn(c.id);
            const installKey = `install:${c.id}`;
            const uninstallKey = `uninstall:${c.id}`;
            const installBusy = busy === installKey;
            const uninstallBusy = busy === uninstallKey;
            return (
              <div key={c.id} className="rtk-client-row">
                <span className={`dot-led ${on ? "ok" : "idle"}`} />
                <div className="rtk-client-labels">
                  <span className="rtk-client-name">{c.label}</span>
                  <span className="rtk-client-sub">{c.sub}</span>
                </div>
                <span className="rtk-client-state">
                  {on ? "configured" : "not configured"}
                </span>
                <div className="rtk-client-actions">
                  <button
                    type="button"
                    className="ping-btn"
                    onClick={() => runAction("install", c.id)}
                    disabled={!status?.installed || installBusy}
                  >
                    {installBusy ? "…" : on ? "reinstall" : "install"}
                  </button>
                  <button
                    type="button"
                    className="reveal-btn"
                    onClick={() => runAction("uninstall", c.id)}
                    disabled={!status?.installed || !on || uninstallBusy}
                  >
                    {uninstallBusy ? "…" : "uninstall"}
                  </button>
                </div>
              </div>
            );
          })}
          {actionLog && (
            <pre className="rtk-action-log">{actionLog}</pre>
          )}
        </div>

        {/* ── 3. Stats hero ──────────────────────────────────────── */}
        <div className="stat-row" style={{ marginTop: 8 }}>
          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">累计命令 · Calls</span>
            </div>
            <div className="stat-tile-display">{nf(gain?.total_commands ?? 0)}</div>
            <div className="stat-tile-sub">
              {gain && gain.avg_time_ms > 0
                ? `avg ${gain.avg_time_ms} ms / call · total ${gain.total_time_ms} ms`
                : "no rewrites recorded"}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">节省 token · Tokens Saved</span>
            </div>
            <div className="stat-tile-display">
              {savedFmt.num}
              <span className="slash"> {savedFmt.unit}</span>
            </div>
            <div className="stat-tile-sub">
              {gain
                ? `${nf(gain.total_input)} in → ${nf(gain.total_output)} out`
                : "—"}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">平均节省 · Avg Savings</span>
            </div>
            <div className="stat-tile-display">
              <span className="accent" style={{ fontSize: "1em", margin: 0 }}>
                {(gain?.avg_savings_pct ?? 0).toFixed(1)}
              </span>
              <span className="slash" style={{ fontSize: "0.55em" }}>%</span>
            </div>
            <div className="stat-tile-sub">
              of upstream output bytes filtered
            </div>
          </div>
        </div>

        {/* ── 4. Recent activity ─────────────────────────────────── */}
        <div className="rtk-history-section">
          <div className="rtk-history-cap">
            <span className="smallcaps">最近 {HISTORY_LIMIT} 次 · Recent Rewrites</span>
            <span className="muted" style={{ fontSize: 10, letterSpacing: "0.06em" }}>
              {history.length} ROWS · {REFRESH_MS / 1000}s REFRESH
            </span>
          </div>

          {history.length === 0 ? (
            <div className="empty">
              No tracking data yet
              <span className="smallcaps">
                restart Claude Code / codex and run{" "}
                <code>git status</code> to see RTK fire
              </span>
            </div>
          ) : (
            <div className="logs-table rtk-history-table">
              <div className="log-row header rtk-history-row">
                <span>Time</span>
                <span>Command</span>
                <span style={{ textAlign: "right" }}>Bytes</span>
                <span style={{ textAlign: "right" }}>Saved</span>
                <span style={{ textAlign: "right" }}>Time</span>
              </div>
              {history.map((row, i) => {
                const negative = row.savings_pct < 0;
                return (
                  <div key={i} className="log-row rtk-history-row">
                    <span className="ts">{shortTime(row.timestamp)}</span>
                    <span className="tool-name" title={row.command}>
                      {truncCommand(row.command)}
                    </span>
                    <span className="bytes" style={{ textAlign: "right" }}>
                      {fmtBytes(row.input_bytes)}
                      <span className="muted">↘</span>
                      {fmtBytes(row.output_bytes)}
                    </span>
                    <span
                      className="bytes"
                      style={{
                        textAlign: "right",
                        color: negative ? "var(--err)" : "var(--warm)",
                      }}
                    >
                      {row.savings_pct.toFixed(0)}%
                    </span>
                    <span
                      className="dur"
                      style={{ textAlign: "right", paddingRight: 0 }}
                    >
                      {row.time_ms}ms
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* ── 5. Hint footer ─────────────────────────────────────── */}
        <div className="rtk-hint">
          <p className="muted" style={{ fontSize: 11, lineHeight: 1.6, margin: 0 }}>
            RTK 在 Bash 工具调用前重写命令（如 <code>git status</code> →{" "}
            <code>rtk git status</code>），输出过滤后再返回给 agent。codex 那边走
            AGENTS.md 提示而不是 hook（codex CLI 没 hook 协议）。
          </p>
          <button
            type="button"
            className="sidebar-link"
            style={{ marginTop: 6 }}
            onClick={() =>
              invoke("open_url", { url: "https://github.com/rtk-ai/rtk" }).catch(
                () => window.open("https://github.com/rtk-ai/rtk", "_blank"),
              )
            }
          >
            <span>github / rtk-ai/rtk</span>
            <span className="sidebar-link-arrow">↗</span>
          </button>
        </div>
      </div>
    </div>
  );
}
