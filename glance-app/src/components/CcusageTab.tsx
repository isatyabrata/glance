import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavEntry } from "../App";
import type {
  CcusageDailyEntry,
  CcusageDailyResponse,
  CcusageSessionEntry,
  CcusageSource,
  CcusageStatus,
} from "../types";
import { TabHeader } from "./TabHeader";

interface Props {
  active: NavEntry;
}

const DAILY_DAYS = 30;
const SESSION_LIMIT = 30;
// ccusage scans hundreds of jsonl files; per-call cost runs ~2-5s so we keep
// background polling lazy. 60 s is a calm cadence for a "how am I doing
// today" tab.
const REFRESH_MS = 60_000;

type SourceFilter = "all" | CcusageSource;

function nf(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

function fmtTokensCompact(n: number): { num: string; unit: string } {
  if (n < 1_000) return { num: `${n}`, unit: "" };
  if (n < 1_000_000) return { num: (n / 1_000).toFixed(1), unit: "K" };
  if (n < 1_000_000_000) return { num: (n / 1_000_000).toFixed(2), unit: "M" };
  return { num: (n / 1_000_000_000).toFixed(2), unit: "B" };
}

function fmtUsd(n: number): string {
  if (n === 0) return "0";
  if (n < 0.01) return n.toFixed(4);
  if (n < 1) return n.toFixed(3);
  if (n < 100) return n.toFixed(2);
  return n.toFixed(0);
}

function shortDate(d: string): string {
  // ccusage emits "YYYY-MM-DD" already; just trim the year for compactness.
  if (/^\d{4}-\d{2}-\d{2}$/.test(d)) return d.slice(5);
  return d;
}

// (No hand-rolled API-equivalent pricing — `@ccusage/codex` ships real
// gpt-5.1-codex pricing via LiteLLM. We rely on its `costUSD` directly.)

function shortProject(p: string, max = 36): string {
  if (!p || p === "Unknown Project") return "—";
  // Project paths arrive as `-Users-xiaojian-Documents-...-codex-switcher`
  // (encoded cwd) or as `<encoded>/<thread-uuid>`. Drop everything before
  // the last meaningful segment.
  const stripped = p.replace(/^-Users-[^-]+-/, "").replace(/^-/, "");
  if (stripped.length <= max) return stripped;
  return "…" + stripped.slice(-(max - 1));
}

const SOURCE_DOT: Record<CcusageSource, string> = {
  claude: "ok",
  codex: "warm",
  mixed: "warm",
  none: "idle",
};

const SOURCE_LABEL: Record<CcusageSource, string> = {
  claude: "claude",
  codex: "codex",
  mixed: "mixed",
  none: "—",
};

// Default OFF — opt-in only. ccusage shells out to `npx -y ccusage` AND
// `npx -y @ccusage/codex` in parallel, each scanning hundreds of JSONL
// files. With auto-poll every 60s + cold npx cache + slow scans they
// stack up: one user reported load avg 115 + 36 GB swap from a runaway
// pile of node ccusage processes. Now: tab loads in disabled state with
// a banner; user clicks once to scan; no auto-poll.
const ENABLED_KEY = "glance.ccusage.enabled";

export function CcusageTab({ active }: Props) {
  const [enabled, setEnabled] = useState<boolean>(() => {
    return localStorage.getItem(ENABLED_KEY) === "1";
  });
  const [status, setStatus] = useState<CcusageStatus | null>(null);
  const [daily, setDaily] = useState<CcusageDailyResponse | null>(null);
  const [sessions, setSessions] = useState<CcusageSessionEntry[]>([]);
  const [topErr, setTopErr] = useState<string | null>(null);
  const [loading, setLoading] = useState<boolean>(false);
  const [filter, setFilter] = useState<SourceFilter>("all");
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      // Status comes back fast (only counts files); kick it off first so we
      // can show the install banner even when the heavier calls are in flight.
      const s = await invoke<CcusageStatus>("ccusage_status");
      setStatus(s);

      // Fetch BOTH ccusage (claude) and @ccusage/codex (codex) in parallel,
      // then merge. ccusage doesn't scan ~/.codex/sessions; @ccusage/codex
      // does AND has real gpt-5.1-codex pricing — so we can't fake codex
      // data via the heuristic source classification. Each row's source
      // tag now reflects which CLI tool produced it.
      const [claudeDaily, claudeSessions, codexDaily, codexSessions] =
        await Promise.all([
          invoke<CcusageDailyResponse>("ccusage_daily", {
            days: DAILY_DAYS,
          }).catch(() => null as CcusageDailyResponse | null),
          invoke<CcusageSessionEntry[]>("ccusage_sessions", {
            limit: SESSION_LIMIT,
          }).catch(() => [] as CcusageSessionEntry[]),
          invoke<CcusageDailyResponse>("ccusage_codex_daily", {
            days: DAILY_DAYS,
          }).catch(() => null as CcusageDailyResponse | null),
          invoke<CcusageSessionEntry[]>("ccusage_codex_sessions", {
            limit: SESSION_LIMIT,
          }).catch(() => [] as CcusageSessionEntry[]),
        ]);

      // Merge daily: rows are tagged with `source` server-side
      // (claude / codex). When the same date exists in both feeds, keep
      // them as separate rows so the bar chart can stack / filter.
      const mergedEntries: CcusageDailyEntry[] = [
        ...(claudeDaily?.entries ?? []),
        ...(codexDaily?.entries ?? []),
      ].sort((a, b) => a.date.localeCompare(b.date));
      const mergedDaily: CcusageDailyResponse = {
        entries: mergedEntries,
        total_input_tokens:
          (claudeDaily?.total_input_tokens ?? 0) +
          (codexDaily?.total_input_tokens ?? 0),
        total_output_tokens:
          (claudeDaily?.total_output_tokens ?? 0) +
          (codexDaily?.total_output_tokens ?? 0),
        total_cache_creation_tokens:
          (claudeDaily?.total_cache_creation_tokens ?? 0) +
          (codexDaily?.total_cache_creation_tokens ?? 0),
        total_cache_read_tokens:
          (claudeDaily?.total_cache_read_tokens ?? 0) +
          (codexDaily?.total_cache_read_tokens ?? 0),
        total_tokens:
          (claudeDaily?.total_tokens ?? 0) + (codexDaily?.total_tokens ?? 0),
        total_cost_usd:
          (claudeDaily?.total_cost_usd ?? 0) +
          (codexDaily?.total_cost_usd ?? 0),
      };
      setDaily(mergedDaily);

      const mergedSessions: CcusageSessionEntry[] = [
        ...claudeSessions,
        ...codexSessions,
      ].sort((a, b) => b.last_activity.localeCompare(a.last_activity));
      // Re-trim after merge so the table still respects SESSION_LIMIT.
      setSessions(mergedSessions.slice(0, SESSION_LIMIT));

      setTopErr(null);
      setLastRefresh(new Date());
    } catch (e) {
      setTopErr(`${e}`);
    } finally {
      setLoading(false);
    }
  }, []);

  // Refresh ONLY when the user has explicitly enabled this tab. No auto-
  // polling — the prior 60s interval × 4 parallel npx invocations was
  // the root cause of the load-avg-115 incident.
  useEffect(() => {
    if (!enabled) return;
    refresh();
    // Note: deliberately NOT setting up a setInterval. Manual refresh only.
  }, [enabled, refresh]);

  function toggleEnabled() {
    const next = !enabled;
    setEnabled(next);
    localStorage.setItem(ENABLED_KEY, next ? "1" : "0");
  }

  const filteredEntries = useMemo<CcusageDailyEntry[]>(() => {
    if (!daily) return [];
    if (filter === "all") return daily.entries;
    return daily.entries.filter((e) => e.source === filter);
  }, [daily, filter]);

  const filteredSessions = useMemo<CcusageSessionEntry[]>(() => {
    if (filter === "all") return sessions;
    return sessions.filter((s) => s.source === filter);
  }, [sessions, filter]);

  const heroStats = useMemo(() => {
    const tokens = filteredEntries.reduce((acc, e) => acc + e.total_tokens, 0);
    const cost = filteredEntries.reduce(
      (acc, e) => acc + e.estimated_cost_usd,
      0,
    );
    // Distinct active dates (rows can duplicate per source on same date).
    const activeDates = new Set(
      filteredEntries.filter((e) => e.total_tokens > 0).map((e) => e.date),
    );
    const days = activeDates.size || 1;
    return { tokens, cost, dailyAvgTokens: tokens / days, days };
  }, [filteredEntries]);

  // Find the bar-chart scale: largest day in the visible window.
  const maxDailyTokens = useMemo(() => {
    let m = 0;
    for (const e of filteredEntries) {
      if (e.total_tokens > m) m = e.total_tokens;
    }
    return m;
  }, [filteredEntries]);

  const tokensFmt = fmtTokensCompact(heroStats.tokens);
  const dailyAvgFmt = fmtTokensCompact(Math.round(heroStats.dailyAvgTokens));

  const headerPill = useMemo(() => {
    if (!status) return { text: "checking", tone: "idle" as const };
    if (!status.installed && (status.error?.length ?? 0) > 0) {
      return { text: "not installed", tone: "err" as const };
    }
    return {
      text: status.version ? `v${status.version}` : "ready",
      tone: "ok" as const,
    };
  }, [status]);

  const overallDot: "ok" | "err" | "idle" | "warm" = !status
    ? "idle"
    : status.installed
    ? "ok"
    : "err";

  const meta = (
    <>
      <span>
        TOKENS <b>{tokensFmt.num}{tokensFmt.unit}</b>
      </span>
      <span>
        COST <b>${fmtUsd(heroStats.cost)}</b>
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} pill={headerPill} meta={meta} />

      <div className="tab-body">
        {topErr && <div className="error-line">{topErr}</div>}

        {/* ── Disabled-by-default banner ─────────────────────────── */}
        {!enabled && (
          <div
            className="upstream-card"
            style={{
              borderTop: "1px solid var(--warm-rule)",
              background: "var(--warm-tint)",
            }}
          >
            <div className="upstream-card-head">
              <span className="dot-led idle" />
              <span className="upstream-card-name">ccusage 默认关闭</span>
              <span className="upstream-card-kind">opt-in</span>
            </div>
            <div className="upstream-card-detail">
              扫描 <code>~/.claude/projects/**/*.jsonl</code> +{" "}
              <code>~/.codex/sessions/**/*.jsonl</code> 通过 4 路并行{" "}
              <code>npx -y ccusage / @ccusage/codex</code>。**真扫起来很吃
              内存**——历史上有过 load avg 115 + 36 GB swap 的事故，因为
              npx 冷启动 × 数百 JSONL × 自动轮询。所以现在：
              <ul style={{ margin: "6px 0 0 16px", paddingLeft: 0 }}>
                <li>默认关闭，需要看的时候点开 ↓</li>
                <li>手动 refresh，不再自动 60 s 轮询</li>
                <li>启用后 tab 不会自动加载历史，每次都要点 refresh</li>
              </ul>
            </div>
            <div style={{ display: "flex", gap: 12, marginTop: 10 }}>
              <button
                type="button"
                className="ping-btn"
                onClick={toggleEnabled}
              >
                启用 ccusage（开始扫描）
              </button>
              <span className="muted" style={{ alignSelf: "center" }}>
                扫描通常 5-30 秒，期间 4 个 node 进程会跑
              </span>
            </div>
          </div>
        )}

        {enabled && (
          <div
            className="muted"
            style={{ fontSize: 11, padding: "4px 0", textAlign: "right" }}
          >
            ⚠ 扫描会临时拉起 4 个 node ccusage 进程 ·{" "}
            <button
              type="button"
              className="reveal-btn"
              onClick={toggleEnabled}
              style={{ display: "inline" }}
            >
              关闭扫描
            </button>
          </div>
        )}

        {/* ── 1. Status card ─────────────────────────────────────── */}
        <div
          className="upstream-card"
          style={{ borderTop: "1px solid var(--rule)" }}
        >
          <div className="upstream-card-head">
            <span className={`dot-led ${overallDot}`} />
            <span className="upstream-card-name">ccusage</span>
            <span className="upstream-card-kind">
              {status?.version ? `v${status.version}` : "—"}
            </span>
            <span className="upstream-card-status">
              {!status
                ? "checking…"
                : status.installed
                ? "via npx"
                : "npx ccusage failed"}
            </span>
            <span className="upstream-card-tools">
              {status
                ? `${nf(status.claude_jsonl_count)} claude · ${nf(
                    status.codex_jsonl_count,
                  )} codex jsonl`
                : "—"}
            </span>
          </div>
          <div className="upstream-card-detail">
            ryoppippi/ccusage scans{" "}
            <code>~/.claude/projects/**/*.jsonl</code> +{" "}
            <code>~/.codex/sessions/**/*.jsonl</code>. No install required —
            <code>npx -y ccusage@latest</code> caches the binary on first run.
          </div>
          {status?.error && (
            <div className="upstream-card-err">⚠ {status.error}</div>
          )}
          <div className="ccusage-toolbar">
            <div className="ccusage-filters">
              {(["all", "claude", "codex", "mixed"] as SourceFilter[]).map(
                (f) => (
                  <button
                    key={f}
                    type="button"
                    className={`upstream-form-tab ${filter === f ? "active" : ""}`}
                    onClick={() => setFilter(f)}
                  >
                    {f}
                  </button>
                ),
              )}
            </div>
            <div className="ccusage-toolbar-right">
              <span className="muted ccusage-refresh-meta">
                {lastRefresh
                  ? `refreshed ${lastRefresh.toLocaleTimeString("zh-CN", {
                      hour12: false,
                    })}`
                  : "—"}
              </span>
              <button
                type="button"
                className="ping-btn"
                onClick={refresh}
                disabled={loading}
              >
                {loading ? "…" : "refresh"}
              </button>
            </div>
          </div>
        </div>

        {/* ── 2. Stats hero ──────────────────────────────────────── */}
        <div className="stat-row" style={{ marginTop: 8 }}>
          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">
                总 token · Total Tokens ({DAILY_DAYS}d)
              </span>
            </div>
            <div className="stat-tile-display">
              {tokensFmt.num}
              {tokensFmt.unit && (
                <span className="slash"> {tokensFmt.unit}</span>
              )}
            </div>
            <div className="stat-tile-sub">
              {filteredEntries.length} days · filter {filter}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">总成本 · Total Cost USD</span>
            </div>
            <div className="stat-tile-display">
              <span className="accent" style={{ fontSize: "1em", margin: 0 }}>
                ${fmtUsd(heroStats.cost)}
              </span>
            </div>
            <div className="stat-tile-sub">
              {(() => {
                // Both `ccusage` (claude) and `@ccusage/codex` (codex) ship
                // real per-token pricing via LiteLLM. No more "API equivalent"
                // hand-waving — the cost number is real either way.
                if (filter === "all" && daily) {
                  return `lifetime $${fmtUsd(daily.total_cost_usd)}`;
                }
                if (filter === "claude" || filter === "codex") {
                  return `filter ↾ ${filter}`;
                }
                return daily
                  ? `lifetime $${fmtUsd(daily.total_cost_usd)}`
                  : "—";
              })()}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">日均 · Daily Avg</span>
            </div>
            <div className="stat-tile-display">
              {heroStats.days <= 1 ? (
                <span style={{ color: "var(--ink-faint)" }}>—</span>
              ) : (
                <>
                  {dailyAvgFmt.num}
                  {dailyAvgFmt.unit && (
                    <span className="slash"> {dailyAvgFmt.unit}</span>
                  )}
                </>
              )}
            </div>
            <div className="stat-tile-sub">
              {heroStats.days <= 1
                ? `仅 ${heroStats.days} 个活跃日，无均值意义`
                : `tokens / active day · n=${heroStats.days}`}
            </div>
          </div>
        </div>

        {/* ── 3. Per-day bar chart ──────────────────────────────── */}
        <div className="rtk-history-section">
          <div className="rtk-history-cap">
            <span className="smallcaps">
              过去 {DAILY_DAYS} 天 · Daily Tokens
            </span>
            <span
              className="muted"
              style={{ fontSize: 10, letterSpacing: "0.06em" }}
            >
              {filteredEntries.length} ROWS · {REFRESH_MS / 1000}s REFRESH
            </span>
          </div>

          {filteredEntries.length === 0 ? (
            <div className="empty">
              No daily usage in window
              <span className="smallcaps">
                run claude code or codex to populate the jsonl logs
              </span>
            </div>
          ) : (
            <div className="ccusage-chart">
              {filteredEntries.map((e) => {
                const pct =
                  maxDailyTokens > 0
                    ? Math.max(0.5, (e.total_tokens / maxDailyTokens) * 100)
                    : 0;
                const tk = fmtTokensCompact(e.total_tokens);
                return (
                  <div className="ccusage-chart-row" key={e.date}>
                    <span className="ccusage-chart-date">
                      {shortDate(e.date)}
                    </span>
                    <span className={`dot-led ${SOURCE_DOT[e.source]}`} />
                    <div className="ccusage-bar-track">
                      <div
                        className={`ccusage-bar-fill src-${e.source}`}
                        style={{ width: `${pct}%` }}
                      />
                    </div>
                    <span className="ccusage-chart-tokens">
                      {tk.num}
                      <span className="muted">{tk.unit}</span>
                    </span>
                    <span className="ccusage-chart-cost">
                      ${fmtUsd(e.estimated_cost_usd)}
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* ── 4. Per-session table ──────────────────────────────── */}
        <div className="rtk-history-section">
          <div className="rtk-history-cap">
            <span className="smallcaps">
              最近 {SESSION_LIMIT} 个会话 · Recent Sessions
            </span>
            <span
              className="muted"
              style={{ fontSize: 10, letterSpacing: "0.06em" }}
            >
              {filteredSessions.length} ROWS
            </span>
          </div>

          {filteredSessions.length === 0 ? (
            <div className="empty">
              No sessions in window
              <span className="smallcaps">
                ccusage finds sessions automatically once jsonl files exist
              </span>
            </div>
          ) : (
            <div className="logs-table ccusage-session-table">
              <div className="log-row header ccusage-session-row">
                <span>Last</span>
                <span>Project</span>
                <span>Source</span>
                <span style={{ textAlign: "right" }}>Tokens</span>
                <span style={{ textAlign: "right" }}>Cost</span>
              </div>
              {filteredSessions.map((s) => {
                const tk = fmtTokensCompact(s.total_tokens);
                return (
                  <div
                    key={`${s.session_id}|${s.project}|${s.last_activity}`}
                    className="log-row ccusage-session-row"
                  >
                    <span className="ts">{shortDate(s.last_activity)}</span>
                    <span
                      className="tool-name"
                      title={`${s.session_id}\n${s.project}`}
                    >
                      {shortProject(s.project) === "—"
                        ? shortProject(s.session_id)
                        : shortProject(s.project)}
                    </span>
                    <span className="ccusage-source-cell">
                      <span className={`dot-led ${SOURCE_DOT[s.source]}`} />
                      <span className="muted">{SOURCE_LABEL[s.source]}</span>
                    </span>
                    <span className="bytes" style={{ textAlign: "right" }}>
                      {tk.num}
                      <span className="muted">{tk.unit}</span>
                    </span>
                    <span
                      className="dur"
                      style={{ textAlign: "right", paddingRight: 0 }}
                    >
                      ${fmtUsd(s.estimated_cost_usd)}
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* ── 5. Hint footer ─────────────────────────────────────── */}
        <div className="rtk-hint">
          <p
            className="muted"
            style={{ fontSize: 11, lineHeight: 1.6, margin: 0 }}
          >
            ccusage 通过解析 <code>~/.claude/projects</code> 和{" "}
            <code>~/.codex/sessions</code> 下的 JSONL 日志计算 token 用量与成本。
            Source 列以模型名区分：<code>claude-*</code> 算作 Claude Code，其他
            （<code>glm-*</code>、<code>gpt-*</code> 等）归入 codex/混合会话。
          </p>
          <button
            type="button"
            className="sidebar-link"
            style={{ marginTop: 6 }}
            onClick={() =>
              invoke("open_url", {
                url: "https://github.com/ryoppippi/ccusage",
              }).catch(() =>
                window.open("https://github.com/ryoppippi/ccusage", "_blank"),
              )
            }
          >
            <span>github / ryoppippi/ccusage</span>
            <span className="sidebar-link-arrow">↗</span>
          </button>
        </div>
      </div>
    </div>
  );
}
