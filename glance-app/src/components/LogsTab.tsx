import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { EventLine } from "../types";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";

interface Props {
  active: NavEntry;
}

const TAIL = 200;

function shortTs(ts: string): string {
  // events.jsonl writes UTC (RFC 3339 with +00:00) — render as local
  // HH:MM:SS so the table matches the user's wall clock.
  const d = new Date(ts);
  if (!isNaN(d.getTime())) return d.toLocaleTimeString("zh-CN", { hour12: false });
  const m = ts.match(/(\d{2}:\d{2}:\d{2})/);
  return m ? m[1] : ts;
}

function fmtDur(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}K`;
  return `${(n / 1024 / 1024).toFixed(1)}M`;
}

type Filter = "all" | "ok" | "err";

export function LogsTab({ active }: Props) {
  const [events, setEvents] = useState<EventLine[]>([]);
  const [filter, setFilter] = useState<Filter>("all");

  const reload = useCallback(async () => {
    try {
      const arr = await invoke<EventLine[]>("tail_events", { n: TAIL });
      setEvents(arr);
    } catch {
      /* ignore */
    }
  }, []);

  useEffect(() => {
    reload();
    const t = window.setInterval(reload, 2000);
    return () => window.clearInterval(t);
  }, [reload]);

  const visible = useMemo(() => {
    if (filter === "all") return events;
    if (filter === "ok") return events.filter((e) => e.ok);
    return events.filter((e) => !e.ok);
  }, [events, filter]);

  async function clearAll() {
    try {
      await invoke("clear_events");
      setEvents([]);
    } catch {
      /* ignore */
    }
  }

  const okCount = events.filter((e) => e.ok).length;
  const errCount = events.length - okCount;

  const meta = (
    <>
      <span>
        SHOWING <b>{visible.length}</b> / {events.length}
      </span>
      <span>
        TAIL <b>{TAIL}</b> · 2s refresh
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        <div className="logs-bar">
          <div className="filter-pills">
            <button
              type="button"
              className={`filter-pill ${filter === "all" ? "active" : ""}`}
              onClick={() => setFilter("all")}
            >
              All · {events.length}
            </button>
            <button
              type="button"
              className={`filter-pill ${filter === "ok" ? "active" : ""}`}
              onClick={() => setFilter("ok")}
            >
              <span className="glyph ok">✓</span> ok · {okCount}
            </button>
            <button
              type="button"
              className={`filter-pill ${filter === "err" ? "active" : ""}`}
              onClick={() => setFilter("err")}
            >
              <span className="glyph err">✗</span> err · {errCount}
            </button>
          </div>
          <div className="logs-actions">
            <button type="button" className="logs-action" onClick={reload}>
              Refresh
            </button>
            <button
              type="button"
              className="logs-action danger"
              onClick={clearAll}
              disabled={events.length === 0}
            >
              Clear logs
            </button>
          </div>
        </div>

        {visible.length === 0 ? (
          <div className="empty">
            no events.
            <span className="smallcaps">enable events_enabled in 状态 / status</span>
          </div>
        ) : (
          <div className="logs-table">
            <div className="log-row header">
              <span>Time</span>
              <span>Tool</span>
              <span style={{ textAlign: "right", paddingRight: 8 }}>Dur</span>
              <span style={{ textAlign: "center" }}>·</span>
              <span>Bytes</span>
              <span>Savings</span>
            </div>
            {visible
              .slice()
              .reverse()
              .map((e, i) => {
                const negativeSavings = e.savings_pct < 0;
                const pct = Math.max(0, Math.min(100, e.savings_pct));
                return (
                  <div key={i} className={`log-row ${e.ok ? "" : "err"}`}>
                    <span className="ts">{shortTs(e.ts)}</span>
                    <span className="tool-name">{e.tool}</span>
                    <span className="dur">{fmtDur(e.duration_ms)}</span>
                    <span
                      className={`ok-cell ${e.ok ? "ok" : "err"}`}
                      title={e.error || ""}
                    >
                      {e.ok ? "✓" : "✗"}
                    </span>
                    <span className="bytes">
                      {fmtBytes(e.bytes_in)}↘{fmtBytes(e.bytes_out)}
                    </span>
                    <span className="savings-bar">
                      <span style={{ minWidth: 30, textAlign: "right" }}>
                        {e.savings_pct.toFixed(0)}%
                      </span>
                      <span className="track">
                        <span
                          className={`fill ${negativeSavings ? "neg" : ""}`}
                          style={{
                            width: `${negativeSavings ? Math.min(100, -e.savings_pct) : pct}%`,
                          }}
                        />
                      </span>
                    </span>
                  </div>
                );
              })}
          </div>
        )}
      </div>
    </div>
  );
}
