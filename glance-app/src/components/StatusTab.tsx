import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { BackendCheck, EventLine, TodayStats } from "../types";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";
import { Sparkline } from "./Sparkline";

interface Props {
  cfg: ReturnType<typeof import("../hooks/useConfig").useConfig>;
  active: NavEntry;
}

function fmtBytes(n: number): { num: string; unit: string } {
  if (n < 1024) return { num: `${n}`, unit: "B" };
  if (n < 1024 * 1024) return { num: (n / 1024).toFixed(1), unit: "KB" };
  if (n < 1024 * 1024 * 1024) return { num: (n / 1024 / 1024).toFixed(2), unit: "MB" };
  return { num: (n / 1024 / 1024 / 1024).toFixed(2), unit: "GB" };
}

function nf(n: number): string {
  return new Intl.NumberFormat("en-US").format(n);
}

/** Compact display for token totals: 12.3K / 4.5M / 1.2B. Anything under
 *  1000 stays as-is so a fresh install showing 47 tokens looks honest. */
function fmtTokens(n: number): { num: string; unit: string } {
  if (n < 1_000) return { num: `${n}`, unit: "" };
  if (n < 1_000_000) return { num: (n / 1_000).toFixed(1), unit: "K" };
  if (n < 1_000_000_000) return { num: (n / 1_000_000).toFixed(2), unit: "M" };
  return { num: (n / 1_000_000_000).toFixed(2), unit: "B" };
}

function timeAgo(ts: number): string {
  const s = Math.max(1, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function StatusTab({ cfg, active }: Props) {
  const [check, setCheck] = useState<BackendCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [stats, setStats] = useState<TodayStats | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number>(Date.now());
  const [latencies, setLatencies] = useState<number[]>([]);
  const [recentEvents, setRecentEvents] = useState<EventLine[]>([]);

  const refreshStats = useCallback(async () => {
    try {
      const s = await invoke<TodayStats>("today_stats");
      setStats(s);
      setUpdatedAt(Date.now());
      const ev = await invoke<EventLine[]>("tail_events", { n: 80 });
      setRecentEvents(ev);
    } catch {
      /* ignore */
    }
  }, []);

  const ping = useCallback(async () => {
    setChecking(true);
    try {
      const r = await invoke<BackendCheck>("test_backend");
      setCheck(r);
      setLatencies((prev) => [...prev.slice(-23), r.latency_ms].filter(Boolean));
    } catch (e) {
      setCheck({
        ok: false,
        status: 0,
        model: "",
        latency_ms: 0,
        error: `${e}`,
      });
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    refreshStats();
    ping();
    const t = window.setInterval(refreshStats, 5000);
    return () => window.clearInterval(t);
  }, [refreshStats, ping]);

  const c = cfg.cfg;

  const peakAndAvg = useMemo(() => {
    if (recentEvents.length === 0) return { peak: null as string | null, avg: 0 };
    const buckets: Record<string, number> = {};
    for (const e of recentEvents) {
      const m = e.ts.match(/T(\d{2}:\d{2})/);
      if (!m) continue;
      buckets[m[1]] = (buckets[m[1]] || 0) + 1;
    }
    let peak: string | null = null;
    let max = 0;
    for (const [k, v] of Object.entries(buckets)) {
      if (v > max) {
        max = v;
        peak = k;
      }
    }
    const avg = Math.round(recentEvents.length / Math.max(Object.keys(buckets).length || 1, 1));
    return { peak, avg };
  }, [recentEvents]);

  const inFmt = fmtBytes(stats?.bytes_in ?? 0);
  const outFmt = fmtBytes(stats?.bytes_out ?? 0);
  const errRate =
    stats && stats.calls > 0 ? (stats.err_count / stats.calls) * 100 : 0;

  const glmTotal = stats?.glm_total_tokens ?? 0;
  const glmAvg = stats?.glm_avg_per_call ?? 0;
  const glmBilled = stats?.glm_billed_calls ?? 0;
  const glmFmt = fmtTokens(glmTotal);
  // Saved bytes = bytes_in - bytes_out (the volume that never reached the
  // calling LLM). Ratio = GLM tokens burned per saved byte. Lower is better.
  const savedBytes = Math.max(0, (stats?.bytes_in ?? 0) - (stats?.bytes_out ?? 0));
  const costRatio = savedBytes > 0 ? glmTotal / savedBytes : 0;

  const headerPill = checking
    ? { text: "checking", tone: "idle" as const }
    : check?.ok
    ? { text: "live", tone: "ok" as const }
    : check === null
    ? { text: "idle", tone: "idle" as const }
    : { text: "down", tone: "err" as const };

  const meta = (
    <>
      <span>
        UPDATED <b>{timeAgo(updatedAt)}</b>
      </span>
      <span>
        CALLS <b>{nf(stats?.calls ?? 0)}</b> TODAY
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} pill={headerPill} meta={meta} />

      <div className="tab-body">
        <div className="hero-row">
          <div className="hero-tile">
            <div className="hero-tile-header">
              <span className="smallcaps">Backend · {(c?.backend.model || "—").toUpperCase()}</span>
            </div>
            <div className="hero-tile-header" style={{ gap: 12 }}>
              <span className={`dot-led ${check?.ok ? "" : check === null ? "idle" : "err"}`} />
              <span className={`hero-tile-display ${check === null ? "" : check.ok ? "" : "err"}`}>
                {check === null ? "checking…" : check.ok ? "reachable." : "unreachable."}
              </span>
            </div>
            <div className="hero-tile-sub">{c?.backend.base_url || "—"}</div>
            {check && !check.ok && check.error && (
              <div className="error-line">{check.error}</div>
            )}
          </div>

          <div className="hero-tile">
            <div className="hero-tile-header">
              <span className="smallcaps">Latency</span>
              <button
                type="button"
                className="reveal-btn"
                style={{ marginLeft: "auto" }}
                onClick={ping}
                disabled={checking}
              >
                {checking ? "pinging" : "re-ping"}
              </button>
            </div>
            <div className="hero-tile-display">
              {check?.latency_ms ? `${check.latency_ms} ms` : "—"}
            </div>
            <Sparkline values={latencies} />
          </div>
        </div>

        <div className="stat-row">
          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">今日调用 · Calls Today</span>
            </div>
            <div className="stat-tile-display">{nf(stats?.calls ?? 0)}</div>
            <div className="stat-tile-sub">
              {peakAndAvg.peak ? `peak ${peakAndAvg.peak}` : "no traffic yet"}
              {peakAndAvg.peak ? ` · ${peakAndAvg.avg}/min avg` : ""}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">字节节省 · Bytes Saved</span>
            </div>
            <div className="stat-tile-display">
              {inFmt.num}
              <span className="accent">↘</span>
              {outFmt.num} <span className="slash">{outFmt.unit}</span>
            </div>
            <div className="stat-tile-sub">
              {(stats?.savings_pct ?? 0).toFixed(1)}% compressed
              {glmTotal > 0 && savedBytes > 0
                ? ` · ${costRatio.toFixed(2)} GLM tok / saved B`
                : ` · in ${inFmt.unit} → out ${outFmt.unit}`}
            </div>
          </div>

          <div
            className="stat-tile"
            title="glance 内部 sub_agent 调用 GLM 烧掉的 token 总和。这是省下 Anthropic token 的对价——便宜池子换贵池子。"
          >
            <div className="stat-tile-cap">
              <span className="smallcaps">GLM 用量 · GLM Tokens</span>
            </div>
            <div className="stat-tile-display">
              {glmFmt.num}
              {glmFmt.unit && <span className="slash"> {glmFmt.unit}</span>}
            </div>
            <div className="stat-tile-sub">
              {glmBilled > 0
                ? `avg ${nf(glmAvg)} / call · ${nf(glmBilled)} billed`
                : "no sub-agent calls yet"}
            </div>
          </div>

          <div className="stat-tile">
            <div className="stat-tile-cap">
              <span className="smallcaps">成功 / 失败 · OK / ERR</span>
            </div>
            <div className="stat-tile-display">
              <span className="ok">{nf(stats?.ok_count ?? 0)}</span>
              <span className="slash"> / </span>
              <span className="err">{nf(stats?.err_count ?? 0)}</span>
            </div>
            <div className="stat-tile-sub">
              {errRate.toFixed(2)}% error rate
            </div>
          </div>
        </div>

        <div className="toggle-row">
          <div className="toggle-row-info">
            <span className="toggle-row-label">Append events to events.jsonl</span>
            <span className="toggle-row-desc">
              Persist every tool call to ~/.glance/events.jsonl — drives the logs view.
            </span>
          </div>
          <label className="toggle">
            <input
              type="checkbox"
              checked={c?.events_enabled ?? false}
              onChange={(e) =>
                cfg.patch((p) => ({ ...p, events_enabled: e.target.checked }), {
                  save: true,
                })
              }
            />
            <span className="toggle-track" />
          </label>
        </div>
      </div>
    </div>
  );
}
