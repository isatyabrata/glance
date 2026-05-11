import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { BackendCheck, GitHubTokenCheck, PingResult, RetryConfig } from "../types";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";

interface Props {
  cfg: ReturnType<typeof import("../hooks/useConfig").useConfig>;
  active: NavEntry;
}

interface FieldDef {
  num: string;
  cn: string;
  en: string;
  key: keyof FormState;
  kind: "text" | "secret" | "number";
}

interface FormState {
  base_url: string;
  api_key: string;
  model: string;
  max_tokens: number;
  timeout_secs: number;
}

const FIELDS: FieldDef[] = [
  { num: "ⅰ", cn: "Base URL", en: "Base URL", key: "base_url", kind: "text" },
  { num: "ⅱ", cn: "API Key", en: "API Key", key: "api_key", kind: "secret" },
  { num: "ⅳ", cn: "上限 tokens", en: "Max Tokens", key: "max_tokens", kind: "number" },
  { num: "ⅴ", cn: "超时 (s)", en: "Timeout (s)", key: "timeout_secs", kind: "number" },
];

function maskKey(k: string): string {
  if (!k) return "(empty)";
  if (k.length <= 8) return "•".repeat(Math.max(k.length, 4));
  return "•".repeat(Math.min(8, k.length - 6)) + k.slice(-6);
}

type PingState =
  | { phase: "idle" }
  | { phase: "running" }
  | { phase: "ok"; ms: number }
  | { phase: "err"; msg: string };

export function BackendTab({ cfg, active }: Props) {
  const [showKey, setShowKey] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<{ tone: "ok" | "err" | "idle"; text: string } | null>(null);

  // Available models fetched from {base_url}/models. Empty = not loaded yet OR
  // endpoint failed; UI falls back to free-text input in that case.
  const [models, setModels] = useState<string[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);

  // Per-model ping status (main + each fallback). Keyed by model id.
  const [pings, setPings] = useState<Record<string, PingState>>({});

  // Fallback adder selection (controlled <select>).
  const [fallbackPick, setFallbackPick] = useState("");

  // GitHub token state.
  const [showGhKey, setShowGhKey] = useState(false);
  const [ghCheck, setGhCheck] = useState<{ tone: "ok" | "err" | "idle"; text: string } | null>(
    null,
  );
  const [ghChecking, setGhChecking] = useState(false);

  const c = cfg.cfg;

  // Refetch models whenever base_url / api_key change (debounced via effect).
  useEffect(() => {
    if (!c) return;
    if (!c.backend.base_url || !c.backend.api_key) return;
    let cancelled = false;
    setModelsLoading(true);
    setModelsError(null);
    invoke<string[]>("list_models")
      .then((list) => {
        if (cancelled) return;
        setModels(list);
        setModelsLoading(false);
      })
      .catch((e) => {
        if (cancelled) return;
        setModels([]);
        setModelsError(String(e).slice(0, 200));
        setModelsLoading(false);
      });
    return () => {
      cancelled = true;
    };
    // intentionally re-run on these two only; rest of cfg isn't load-bearing
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [c?.backend.base_url, c?.backend.api_key]);

  if (!c) return <div className="muted">Loading…</div>;

  function update<K extends keyof FormState>(key: K, value: FormState[K]) {
    cfg.patch((p) => ({
      ...p,
      backend: { ...p.backend, [key]: value },
    }));
  }

  function commit() {
    cfg.save().catch(() => {});
  }

  async function reloadModels() {
    setModelsLoading(true);
    setModelsError(null);
    try {
      const list = await invoke<string[]>("list_models");
      setModels(list);
    } catch (e) {
      setModels([]);
      setModelsError(String(e).slice(0, 200));
    } finally {
      setModelsLoading(false);
    }
  }

  async function pingOne(model: string) {
    setPings((p) => ({ ...p, [model]: { phase: "running" } }));
    try {
      // Save first so the backend command sees the latest base_url / api_key.
      await cfg.save();
      const r = await invoke<PingResult>("ping_model", { model });
      if (r.ok) {
        setPings((p) => ({ ...p, [model]: { phase: "ok", ms: r.latency_ms } }));
      } else {
        setPings((p) => ({
          ...p,
          [model]: { phase: "err", msg: r.error || `HTTP ${r.status}` },
        }));
      }
    } catch (e) {
      setPings((p) => ({ ...p, [model]: { phase: "err", msg: String(e).slice(0, 80) } }));
    }
  }

  function addFallback() {
    const v = fallbackPick.trim();
    if (!v) return;
    cfg.patch((p) => ({
      ...p,
      backend: {
        ...p.backend,
        fallback_models: Array.from(new Set([...(p.backend.fallback_models ?? []), v])),
      },
    }));
    setFallbackPick("");
    cfg.save().catch(() => {});
  }

  function removeFallback(v: string) {
    cfg.patch((p) => ({
      ...p,
      backend: {
        ...p.backend,
        fallback_models: (p.backend.fallback_models ?? []).filter((x) => x !== v),
      },
    }));
    cfg.save().catch(() => {});
  }

  function updateGithubToken(v: string) {
    cfg.patch((p) => ({
      ...p,
      tokens: { ...(p.tokens ?? { github: "" }), github: v },
    }));
  }

  async function testGithubToken() {
    setGhChecking(true);
    setGhCheck({ tone: "idle", text: "saving + dialing GitHub…" });
    try {
      await cfg.save();
      const r = await invoke<GitHubTokenCheck>("test_github_token");
      if (r.ok) {
        const scopes = r.scopes.length ? r.scopes.join(", ") : "(no scopes / fine-grained PAT)";
        setGhCheck({
          tone: "ok",
          text: `✓ ${r.latency_ms} ms · @${r.login ?? "?"} · scopes: ${scopes}`,
        });
      } else {
        setGhCheck({
          tone: "err",
          text: `✗ ${r.status || "fail"} · ${r.error || "unknown error"}`,
        });
      }
    } catch (e) {
      setGhCheck({ tone: "err", text: `✗ ${e}` });
    } finally {
      setGhChecking(false);
    }
  }

  function updateRetry<K extends keyof RetryConfig>(key: K, value: RetryConfig[K]) {
    cfg.patch((p) => ({
      ...p,
      backend: {
        ...p.backend,
        retry: {
          ...(p.backend.retry ?? { max_retries: 3, base_backoff_ms: 1000, max_backoff_secs: 30 }),
          [key]: value,
        },
      },
    }));
  }

  async function test() {
    setTesting(true);
    setResult({ tone: "idle", text: "saving + dialing backend…" });
    try {
      await cfg.save();
    } catch (e) {
      setResult({ tone: "err", text: `save failed · ${e}` });
      setTesting(false);
      return;
    }
    try {
      const r = await invoke<BackendCheck>("test_backend");
      if (r.ok) {
        setResult({
          tone: "ok",
          text: `✓ ${r.latency_ms} ms · ${r.model} confirmed (HTTP ${r.status})`,
        });
      } else {
        setResult({
          tone: "err",
          text: `✗ ${r.status || "fail"} · ${r.error || "unknown error"}`,
        });
      }
    } catch (e) {
      setResult({ tone: "err", text: `✗ ${e}` });
    } finally {
      setTesting(false);
    }
  }

  const meta = (
    <>
      <span>
        FILE <b>~/.glance/config.toml</b>
      </span>
      <span>
        STATE <b>{cfg.saving ? "saving…" : "synced"}</b>
      </span>
    </>
  );

  // Models that are already taken (main + every fallback) — excluded from the
  // fallback adder dropdown so the user can't pick the same model twice.
  const takenModels = new Set<string>([c.backend.model, ...(c.backend.fallback_models ?? [])]);
  const availableForFallback = models.filter((m) => !takenModels.has(m));

  function renderPing(model: string) {
    const p = pings[model] ?? { phase: "idle" };
    return (
      <span className={`ping-mark ping-${p.phase}`}>
        {p.phase === "idle" && (
          <button
            type="button"
            className="ping-btn"
            onClick={() => pingOne(model)}
            title={`ping ${model}`}
          >
            ping
          </button>
        )}
        {p.phase === "running" && <span className="ping-dot">…</span>}
        {p.phase === "ok" && <span className="ping-dot">✓ {p.ms}ms</span>}
        {p.phase === "err" && (
          <span className="ping-dot" title={p.msg}>
            ✗
          </span>
        )}
      </span>
    );
  }

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        <div className="roman-stack">
          {/* Standard fields ⅰ ⅱ ⅳ ⅴ */}
          {FIELDS.slice(0, 2).map((f) => renderField(f, c, update, showKey, setShowKey, commit))}

          {/* ⅲ Model — dropdown when /models is reachable, free-text fallback otherwise */}
          <div className="roman-row" key="model">
            <span className="roman-num">ⅲ</span>
            <div className="roman-label">
              <span className="roman-label-cn">Model</span>
              <span className="roman-label-en">Model</span>
            </div>
            <div className="roman-value model-row">
              {models.length > 0 ? (
                <select
                  className="inline-input model-select"
                  value={c.backend.model}
                  onChange={(e) => {
                    update("model", e.target.value);
                    cfg.save().catch(() => {});
                  }}
                >
                  {!models.includes(c.backend.model) && (
                    <option value={c.backend.model}>
                      {c.backend.model} (not in /models — keep)
                    </option>
                  )}
                  {models.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </select>
              ) : (
                <input
                  className="inline-input"
                  type="text"
                  value={c.backend.model}
                  onChange={(e) => update("model", e.target.value)}
                  onBlur={commit}
                  placeholder={modelsLoading ? "loading models…" : "model name"}
                />
              )}
              <button
                type="button"
                className="reveal-btn"
                onClick={reloadModels}
                disabled={modelsLoading}
                title="reload /models endpoint"
              >
                {modelsLoading ? "…" : "↻"}
              </button>
              {renderPing(c.backend.model)}
            </div>
          </div>

          {FIELDS.slice(2).map((f) => renderField(f, c, update, showKey, setShowKey, commit))}
        </div>

        {modelsError && (
          <p className="muted" style={{ fontSize: 11, marginTop: 6 }}>
            ↻ /models 失败：{modelsError}（用文本框继续编辑也行）
          </p>
        )}

        <section className="chip-section">
          <header className="chip-section-header">
            <span className="chip-section-title">fallback_models</span>
            <span className="chip-section-count">
              {c.backend.fallback_models?.length ?? 0}
            </span>
          </header>
          <p className="chip-section-desc">
            主模型 429 / 5xx 重试用尽后按顺序降级到这些模型，每级独享一份重试预算。
            质量从高到低排，例如 <code>glm-5 → glm-4.7 → glm-4.5-air</code>。空 = 不降级。
          </p>
          <div className="chip-list">
            {(c.backend.fallback_models ?? []).map((m) => (
              <span key={m} className="chip">
                <span className="chip-text">{m}</span>
                {renderPing(m)}
                <button
                  type="button"
                  className="chip-x"
                  onClick={() => removeFallback(m)}
                  aria-label={`remove ${m}`}
                >
                  ×
                </button>
              </span>
            ))}
            {models.length > 0 ? (
              <>
                <select
                  className="inline-input chip-select"
                  value={fallbackPick}
                  onChange={(e) => setFallbackPick(e.target.value)}
                >
                  <option value="">+ pick model…</option>
                  {availableForFallback.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="chip-add"
                  onClick={addFallback}
                  disabled={!fallbackPick}
                >
                  add
                </button>
              </>
            ) : (
              <>
                <input
                  className="chip-input"
                  type="text"
                  value={fallbackPick}
                  onChange={(e) => setFallbackPick(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      addFallback();
                    }
                  }}
                  placeholder="+ glm-5"
                />
                <button type="button" className="chip-add" onClick={addFallback}>
                  add
                </button>
              </>
            )}
          </div>
        </section>

        <section className="retry-section">
          <header className="chip-section-header">
            <span className="chip-section-title">retry policy</span>
            <span className="chip-section-count">
              {c.backend.retry?.max_retries ?? 3}× attempts
            </span>
          </header>
          <p className="chip-section-desc">
            每个模型的重试预算。退避：<code>base × 3^N</code>。 服务器返{" "}
            <code>Retry-After: N</code> 时按它等，封顶 <code>max_backoff_secs</code>。
          </p>
          <div className="retry-grid">
            <label className="retry-field">
              <span className="retry-label">max_retries</span>
              <input
                className="inline-input is-num"
                type="number"
                min={0}
                max={10}
                value={c.backend.retry?.max_retries ?? 3}
                onChange={(e) =>
                  updateRetry("max_retries", Math.max(0, parseInt(e.target.value) || 0))
                }
                onBlur={commit}
              />
            </label>
            <label className="retry-field">
              <span className="retry-label">base_backoff_ms</span>
              <input
                className="inline-input is-num"
                type="number"
                min={100}
                step={100}
                value={c.backend.retry?.base_backoff_ms ?? 1000}
                onChange={(e) =>
                  updateRetry(
                    "base_backoff_ms",
                    Math.max(100, parseInt(e.target.value) || 1000),
                  )
                }
                onBlur={commit}
              />
            </label>
            <label className="retry-field">
              <span className="retry-label">max_backoff_secs</span>
              <input
                className="inline-input is-num"
                type="number"
                min={1}
                max={300}
                value={c.backend.retry?.max_backoff_secs ?? 30}
                onChange={(e) =>
                  updateRetry(
                    "max_backoff_secs",
                    Math.max(1, parseInt(e.target.value) || 30),
                  )
                }
                onBlur={commit}
              />
            </label>
          </div>
        </section>

        <section className="retry-section">
          <header className="chip-section-header">
            <span className="chip-section-title">tokens.github</span>
            <span className="chip-section-count">
              {c.tokens?.github ? "set" : "—"}
            </span>
          </header>
          <p className="chip-section-desc">
            <code>repo_explore</code> 的 GitHub PAT。设了：5000 req/h + 能用 code search；
            没设：60 req/h 匿名，<code>search_doc</code> 自动 fallback 到 zread。
            优先级：<code>GITHUB_TOKEN</code> 环境变量 &gt; 这个字段。fine-grained PAT
            勾 <em>public_repo</em> 读权限就够。
          </p>
          <div className="model-row" style={{ marginTop: 10 }}>
            {showGhKey ? (
              <input
                className="inline-input"
                type="text"
                value={c.tokens?.github ?? ""}
                onChange={(e) => updateGithubToken(e.target.value)}
                onBlur={commit}
                placeholder="ghp_… or github_pat_…"
                style={{ flex: 1 }}
              />
            ) : (
              <input
                className="inline-input"
                type="text"
                readOnly
                value={maskKey(c.tokens?.github ?? "")}
                onClick={() => setShowGhKey(true)}
                style={{ flex: 1, cursor: "pointer", letterSpacing: "0.08em" }}
              />
            )}
            <button
              type="button"
              className="reveal-btn"
              onClick={() => setShowGhKey((s) => !s)}
            >
              {showGhKey ? "hide" : "reveal"}
            </button>
            <button
              type="button"
              className="ping-btn"
              disabled={ghChecking}
              onClick={testGithubToken}
              title="GET https://api.github.com/user with this token"
            >
              {ghChecking ? "…" : "test"}
            </button>
          </div>
          {ghCheck && (
            <div
              className={`test-bar-result ${ghCheck.tone}`}
              style={{ marginTop: 8, fontSize: 11 }}
            >
              {ghCheck.text}
            </div>
          )}
        </section>

        <div className="test-bar">
          <button
            type="button"
            className="test-bar-button"
            disabled={testing || !c.backend.api_key}
            onClick={test}
          >
            <span className="test-bar-num">→</span>
            <span className="test-bar-label">
              {testing ? "Testing connection…" : "Test connection"}
            </span>
            <span className="test-bar-en">CHAT/COMPLETIONS · 1 token</span>
          </button>
          <div className={`test-bar-result ${result?.tone ?? ""}`}>{result?.text ?? " "}</div>
        </div>
      </div>
    </div>
  );
}

// ── helpers ─────────────────────────────────────────────────────────────────

function renderField(
  f: FieldDef,
  c: import("../types").Config,
  update: <K extends keyof FormState>(key: K, value: FormState[K]) => void,
  showKey: boolean,
  setShowKey: React.Dispatch<React.SetStateAction<boolean>>,
  commit: () => void,
) {
  const value = (c.backend as unknown as FormState)[f.key];
  return (
    <div className="roman-row" key={f.key}>
      <span className="roman-num">{f.num}</span>
      <div className="roman-label">
        <span className="roman-label-cn">{f.cn}</span>
        <span className="roman-label-en">{f.en}</span>
      </div>
      <div className="roman-value">
        {f.kind === "secret" ? (
          <>
            {showKey ? (
              <input
                className="inline-input"
                type="text"
                value={String(value)}
                onChange={(e) => update("api_key", e.target.value)}
                onBlur={commit}
              />
            ) : (
              <input
                className="inline-input"
                type="text"
                readOnly
                value={maskKey(String(value))}
                onClick={() => setShowKey(true)}
                style={{ cursor: "pointer", letterSpacing: "0.08em" }}
              />
            )}
            <button
              type="button"
              className="reveal-btn"
              onClick={() => setShowKey((s) => !s)}
            >
              {showKey ? "hide" : "reveal"}
            </button>
          </>
        ) : f.kind === "number" ? (
          <input
            className="inline-input is-num"
            type="number"
            min={1}
            value={Number(value)}
            onChange={(e) =>
              update(f.key, Math.max(1, parseInt(e.target.value) || 1) as never)
            }
            onBlur={commit}
          />
        ) : (
          <input
            className="inline-input"
            type="text"
            value={String(value)}
            onChange={(e) => update(f.key, e.target.value as never)}
            onBlur={commit}
          />
        )}
      </div>
    </div>
  );
}
