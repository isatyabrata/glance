import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";

interface Props {
  cfg: ReturnType<typeof import("../hooks/useConfig").useConfig>;
  active: NavEntry;
}

const ITER_MIN = 4;
const ITER_MAX = 20;

export function SettingsTab({ cfg, active }: Props) {
  const [pathInput, setPathInput] = useState("");
  const [keywordInput, setKeywordInput] = useState("");
  const [err, setErr] = useState<string | null>(null);

  const c = cfg.cfg;
  if (!c) return <div className="muted">Loading…</div>;

  async function pickVault() {
    try {
      const picked = await invoke<string | null>("pick_folder");
      if (picked) {
        cfg.patch(
          (p) => ({ ...p, obsidian: { ...p.obsidian, vault: picked } }),
          { save: true },
        );
      }
    } catch (e) {
      setErr(`${e}`);
    }
  }

  function addDenyPath() {
    const v = pathInput.trim();
    if (!v) return;
    cfg.patch(
      (p) => ({
        ...p,
        safety: {
          ...p.safety,
          deny_paths: Array.from(new Set([...p.safety.deny_paths, v])),
        },
      }),
      { save: true },
    );
    setPathInput("");
  }

  function removeDenyPath(v: string) {
    cfg.patch(
      (p) => ({
        ...p,
        safety: {
          ...p.safety,
          deny_paths: p.safety.deny_paths.filter((x) => x !== v),
        },
      }),
      { save: true },
    );
  }

  function addDenyKeyword() {
    const v = keywordInput.trim();
    if (!v) return;
    cfg.patch(
      (p) => ({
        ...p,
        safety: {
          ...p.safety,
          deny_keywords: Array.from(new Set([...p.safety.deny_keywords, v])),
        },
      }),
      { save: true },
    );
    setKeywordInput("");
  }

  function removeDenyKeyword(v: string) {
    cfg.patch(
      (p) => ({
        ...p,
        safety: {
          ...p.safety,
          deny_keywords: p.safety.deny_keywords.filter((x) => x !== v),
        },
      }),
      { save: true },
    );
  }

  function setIter(n: number) {
    cfg.patch(
      (p) => ({
        ...p,
        sub_agent: { ...p.sub_agent, max_iterations: n },
      }),
      { save: true },
    );
  }

  const iters = c.sub_agent.max_iterations;
  const tickValues: number[] = [];
  for (let i = ITER_MIN; i <= ITER_MAX; i += 1) tickValues.push(i);

  const meta = (
    <>
      <span>
        STATE <b>{cfg.saving ? "saving…" : "synced"}</b>
      </span>
      <span>
        FILE <b>~/.glance/config.toml</b>
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        {err && <div className="error-line">{err}</div>}

        {/* Vault tile */}
        <div className="vault-tile">
          <svg
            className="vault-glyph"
            viewBox="0 0 64 64"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinecap="round"
          >
            <rect x="6" y="14" width="52" height="38" rx="2" />
            <line x1="6" y1="26" x2="58" y2="26" />
            <line x1="6" y1="38" x2="58" y2="38" />
            <circle cx="32" cy="20" r="1" />
            <circle cx="32" cy="32" r="1" />
            <circle cx="32" cy="44" r="1" />
          </svg>
          <div className="vault-info">
            <span className="vault-info-label">Obsidian vault</span>
            <span className="vault-info-path">
              {c.obsidian.vault || "(empty — fallback resolves via AGENTS.md / iCloud)"}
            </span>
          </div>
          <button type="button" className="browse-btn" onClick={pickVault}>
            browse
          </button>
        </div>

        {/* Deny paths */}
        <section className="chip-section">
          <header className="chip-section-header">
            <span className="chip-section-title">deny_paths</span>
            <span className="chip-section-count">{c.safety.deny_paths.length}</span>
          </header>
          <p className="chip-section-desc">
            Path fragments that block read/write tools. e.g. <b>auth</b>, <b>secret</b>, <b>.env</b>.
          </p>
          <div className="chip-list">
            {c.safety.deny_paths.length === 0 && (
              <span className="muted" style={{ fontFamily: "var(--font-mono)", fontSize: 11 }}>
                · empty ·
              </span>
            )}
            {c.safety.deny_paths.map((p) => (
              <span key={p} className="chip">
                {p}
                <button
                  type="button"
                  className="chip-x"
                  onClick={() => removeDenyPath(p)}
                  title={`remove ${p}`}
                >
                  ×
                </button>
              </span>
            ))}
          </div>
          <div className="chip-add-row">
            <input
              className="inline-input"
              type="text"
              placeholder="add path fragment…"
              value={pathInput}
              onChange={(e) => setPathInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") addDenyPath();
              }}
            />
            <button type="button" className="reveal-btn" onClick={addDenyPath}>
              add
            </button>
          </div>
        </section>

        {/* Deny keywords */}
        <section className="chip-section">
          <header className="chip-section-header">
            <span className="chip-section-title">deny_keywords</span>
            <span className="chip-section-count">{c.safety.deny_keywords.length}</span>
          </header>
          <p className="chip-section-desc">
            Keywords that block writes when they appear in target file content.
            e.g. <b>production</b>, <b>encrypt</b>.
          </p>
          <div className="chip-list">
            {c.safety.deny_keywords.length === 0 && (
              <span className="muted" style={{ fontFamily: "var(--font-mono)", fontSize: 11 }}>
                · empty ·
              </span>
            )}
            {c.safety.deny_keywords.map((k) => (
              <span key={k} className="chip">
                {k}
                <button
                  type="button"
                  className="chip-x"
                  onClick={() => removeDenyKeyword(k)}
                  title={`remove ${k}`}
                >
                  ×
                </button>
              </span>
            ))}
          </div>
          <div className="chip-add-row">
            <input
              className="inline-input"
              type="text"
              placeholder="add keyword…"
              value={keywordInput}
              onChange={(e) => setKeywordInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") addDenyKeyword();
              }}
            />
            <button type="button" className="reveal-btn" onClick={addDenyKeyword}>
              add
            </button>
          </div>
        </section>

        {/* Iterations slider */}
        <section className="slider-block">
          <div className="slider-block-header">
            <div>
              <div className="chip-section-title">sub-agent · max_iterations</div>
              <p className="chip-section-desc" style={{ marginTop: 6, marginBottom: 0 }}>
                Function-calling loop ceiling per tool call. Higher = richer answers, more cost.
              </p>
            </div>
          </div>

          <div className="slider-track-wrap">
            <div className="slider-track" />
            <div className="slider-ticks">
              {tickValues.map((v) => {
                const isActive = v <= iters;
                const isCurrent = v === iters;
                return (
                  <span
                    key={v}
                    className={`slider-tick ${isActive ? "active" : ""} ${isCurrent ? "current" : ""}`}
                  >
                    {(v === ITER_MIN || v === ITER_MAX || v % 4 === 0) && (
                      <span className="slider-tick-label">{v}</span>
                    )}
                  </span>
                );
              })}
            </div>
            <input
              className="slider-input"
              type="range"
              min={ITER_MIN}
              max={ITER_MAX}
              step={1}
              value={iters}
              onChange={(e) => setIter(parseInt(e.target.value) || ITER_MIN)}
            />
          </div>

          <div className="slider-current">{iters}</div>
        </section>
      </div>
    </div>
  );
}
