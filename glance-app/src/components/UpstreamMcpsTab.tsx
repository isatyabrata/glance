import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavEntry } from "../App";
import type {
  SmokeTestResult,
  UpstreamMcp,
  UpstreamMcpListEntry,
  UpstreamTemplate,
} from "../types";
import { TabHeader } from "./TabHeader";

interface Props {
  active: NavEntry;
}

type AddMode = "template" | "custom";
type TransportKind = "stdio" | "streamable_http";

const EMPTY_STDIO: UpstreamMcp = {
  type: "stdio",
  name: "",
  command: "",
  args: [],
  env: {},
  enabled: true,
  clients: [],
};

const EMPTY_HTTP: UpstreamMcp = {
  type: "streamable_http",
  name: "",
  url: "",
  api_key: "",
  enabled: true,
  clients: [],
};

const ALL_CLIENTS: import("../types").McpClientId[] = ["claude", "codex", "cursor"];

export function UpstreamMcpsTab({ active }: Props) {
  const [list, setList] = useState<UpstreamMcpListEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [reloading, setReloading] = useState(false);
  const [topErr, setTopErr] = useState<string | null>(null);

  const [adding, setAdding] = useState(false);
  const [addMode, setAddMode] = useState<AddMode>(() => {
    const saved = localStorage.getItem("glance.upstreams.addMode");
    return saved === "custom" ? "custom" : "template";
  });

  const [templates, setTemplates] = useState<UpstreamTemplate[]>([]);
  const [draft, setDraft] = useState<UpstreamMcp>(EMPTY_STDIO);
  const [draftEnvText, setDraftEnvText] = useState("");
  const [draftArgsText, setDraftArgsText] = useState("");
  const [testRes, setTestRes] = useState<SmokeTestResult | null>(null);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);

  useEffect(() => {
    localStorage.setItem("glance.upstreams.addMode", addMode);
  }, [addMode]);

  async function refresh(showSpinner = true) {
    if (showSpinner) setLoading(true);
    try {
      const r = await invoke<UpstreamMcpListEntry[]>("list_upstream_mcps");
      setList(r);
      setTopErr(null);
    } catch (e) {
      setTopErr(`${e}`);
    } finally {
      if (showSpinner) setLoading(false);
    }
  }

  useEffect(() => {
    refresh();
    invoke<UpstreamTemplate[]>("list_upstream_templates")
      .then(setTemplates)
      .catch((e) => setTopErr(`templates: ${e}`));
  }, []);

  async function reloadAggregator() {
    setReloading(true);
    try {
      await invoke("reload_upstream_mcps");
      await refresh(false);
    } catch (e) {
      setTopErr(`${e}`);
    } finally {
      setReloading(false);
    }
  }

  async function toggleEnabled(name: string, next: boolean) {
    try {
      await invoke("set_upstream_mcp_enabled", { args: { name, enabled: next } });
      await refresh(false);
    } catch (e) {
      setTopErr(`${e}`);
    }
  }

  async function removeUpstream(name: string) {
    try {
      await invoke("remove_upstream_mcp", { name });
      setConfirmRemove(null);
      await refresh(false);
    } catch (e) {
      setTopErr(`${e}`);
    }
  }

  function startAdd(mode: AddMode) {
    setAddMode(mode);
    setAdding(true);
    setDraft(EMPTY_STDIO);
    setDraftArgsText("");
    setDraftEnvText("");
    setTestRes(null);
  }

  function applyTemplate(t: UpstreamTemplate) {
    setDraft(t.spec);
    if (t.spec.type === "stdio") {
      setDraftArgsText(t.spec.args.join(" "));
      setDraftEnvText(envToText(t.spec.env));
    } else {
      setDraftArgsText("");
      setDraftEnvText("");
    }
    setTestRes(null);
  }

  function setKind(k: TransportKind) {
    if (k === "stdio") {
      setDraft({ ...EMPTY_STDIO, name: draft.name, enabled: draft.enabled });
    } else {
      setDraft({ ...EMPTY_HTTP, name: draft.name, enabled: draft.enabled });
    }
    setDraftArgsText("");
    setDraftEnvText("");
    setTestRes(null);
  }

  function buildSpecForSubmission(): UpstreamMcp {
    if (draft.type === "stdio") {
      return {
        ...draft,
        args: parseShellArgs(draftArgsText),
        env: parseEnvText(draftEnvText),
      };
    }
    return draft;
  }

  async function runTest() {
    setTesting(true);
    setTestRes(null);
    try {
      const spec = buildSpecForSubmission();
      const r = await invoke<SmokeTestResult>("test_upstream_mcp", { spec });
      setTestRes(r);
    } catch (e) {
      setTestRes({
        name: draft.name,
        ok: false,
        tool_count: 0,
        latency_ms: 0,
        error: `${e}`,
        sample_tools: [],
      });
    } finally {
      setTesting(false);
    }
  }

  async function saveDraft() {
    if (!draft.name.trim()) {
      setTestRes({
        name: "",
        ok: false,
        tool_count: 0,
        latency_ms: 0,
        error: "name is required",
        sample_tools: [],
      });
      return;
    }
    setSaving(true);
    try {
      await invoke("add_upstream_mcp", { spec: buildSpecForSubmission() });
      setAdding(false);
      await refresh(false);
    } catch (e) {
      setTestRes({
        name: draft.name,
        ok: false,
        tool_count: 0,
        latency_ms: 0,
        error: `${e}`,
        sample_tools: [],
      });
    } finally {
      setSaving(false);
    }
  }

  const totalConnected = useMemo(
    () => list.filter((e) => e.runtime?.status === "connected").length,
    [list],
  );
  const totalTools = useMemo(
    () => list.reduce((acc, e) => acc + (e.runtime?.tool_count ?? 0), 0),
    [list],
  );

  const meta = (
    <>
      <span>
        UPSTREAMS <b>{totalConnected}</b> / {list.length}
      </span>
      <span>
        EXPOSED <b>{totalTools}</b> tools
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        {topErr && <div className="error-line">{topErr}</div>}

        <div
          className="model-row"
          style={{ justifyContent: "space-between", marginBottom: 14 }}
        >
          <div className="muted" style={{ fontSize: 11, lineHeight: 1.6 }}>
            Glance aggregates external MCPs under a <b>name__tool</b> namespace.
            Configure once here — every Claude / codex / cursor session that points
            at <code>glance</code> sees them automatically.
          </div>
          <button
            type="button"
            className="reveal-btn"
            onClick={reloadAggregator}
            disabled={reloading}
            title="Re-run initialize for every upstream"
          >
            {reloading ? "…" : "↻ reconnect"}
          </button>
        </div>

        {loading && <div className="muted">Loading…</div>}

        {!loading && list.length === 0 && (
          <div className="upstream-empty">
            <div className="upstream-empty-mark">∅</div>
            <div className="upstream-empty-title">No upstream MCPs configured</div>
            <p className="upstream-empty-desc">
              Add one and glance starts proxying its tools to every connected
              client. Templates cover the usual suspects (context7, playwright,
              zread, web-search-prime…).
            </p>
            <div className="model-row" style={{ justifyContent: "center" }}>
              <button
                type="button"
                className="test-bar-button"
                onClick={() => startAdd("template")}
              >
                <span className="test-bar-num">→</span>
                <span className="test-bar-label">Try a template</span>
                <span className="test-bar-en">7 PRESETS · ONE-CLICK</span>
              </button>
            </div>
          </div>
        )}

        {!loading && list.length > 0 && (
          <div className="upstream-list">
            {list.map((entry) => (
              <UpstreamCard
                key={entry.spec.name}
                entry={entry}
                onToggle={(next) => toggleEnabled(entry.spec.name, next)}
                onClientsChange={async (clients) => {
                  const next = { ...entry.spec, clients } as typeof entry.spec;
                  try {
                    await invoke("add_upstream_mcp", { spec: next });
                    await refresh(false);
                  } catch (e) {
                    setTopErr(`update clients on ${entry.spec.name}: ${e}`);
                    await refresh(false);
                  }
                }}
                onRemove={() => setConfirmRemove(entry.spec.name)}
                onTest={async () => {
                  setTesting(true);
                  try {
                    const r = await invoke<SmokeTestResult>("test_upstream_mcp", {
                      spec: entry.spec,
                    });
                    setTestRes(r);
                  } catch (e) {
                    setTestRes({
                      name: entry.spec.name,
                      ok: false,
                      tool_count: 0,
                      latency_ms: 0,
                      error: `${e}`,
                      sample_tools: [],
                    });
                  } finally {
                    setTesting(false);
                  }
                }}
                confirmingRemove={confirmRemove === entry.spec.name}
                onCancelRemove={() => setConfirmRemove(null)}
              />
            ))}
          </div>
        )}

        {testRes && !adding && (
          <div
            className={`test-bar-result ${testRes.ok ? "ok" : "err"}`}
            style={{ marginTop: 10, fontSize: 11 }}
          >
            {testRes.ok
              ? `✓ ${testRes.name} · ${testRes.tool_count} tools · ${testRes.latency_ms} ms${
                  testRes.sample_tools.length
                    ? ` · ${testRes.sample_tools.join(", ")}`
                    : ""
                }`
              : `✗ ${testRes.name || "(unnamed)"} · ${testRes.error || "unknown error"}`}
          </div>
        )}

        <div className="upstream-add-bar">
          {!adding ? (
            <button
              type="button"
              className="test-bar-button"
              onClick={() => startAdd("template")}
            >
              <span className="test-bar-num">+</span>
              <span className="test-bar-label">Add upstream MCP</span>
              <span className="test-bar-en">TEMPLATE OR CUSTOM</span>
            </button>
          ) : (
            <AddForm
              addMode={addMode}
              setAddMode={setAddMode}
              templates={templates}
              draft={draft}
              setDraft={setDraft}
              draftArgsText={draftArgsText}
              setDraftArgsText={setDraftArgsText}
              draftEnvText={draftEnvText}
              setDraftEnvText={setDraftEnvText}
              setKind={setKind}
              applyTemplate={applyTemplate}
              testRes={testRes}
              testing={testing}
              saving={saving}
              onTest={runTest}
              onSave={saveDraft}
              onCancel={() => {
                setAdding(false);
                setTestRes(null);
              }}
            />
          )}
        </div>

        {confirmRemove && (
          <div className="upstream-confirm">
            <span>
              Remove <b>{confirmRemove}</b>? Its tools disappear immediately.
            </span>
            <div className="model-row">
              <button
                type="button"
                className="reveal-btn"
                onClick={() => setConfirmRemove(null)}
              >
                cancel
              </button>
              <button
                type="button"
                className="ping-btn"
                onClick={() => removeUpstream(confirmRemove)}
              >
                remove
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

interface ClientsRowProps {
  spec: UpstreamMcp;
  runtime: import("../types").UpstreamStatusSnapshot | null;
  onChange: (next: import("../types").McpClientId[]) => Promise<void> | void;
}

function ClientsRow({ spec, runtime, onChange }: ClientsRowProps) {
  // Optimistic local state: flip immediately on click, then await backend.
  // When the parent re-fetches, the prop changes → we resync via useEffect.
  const propClients = (spec.clients ?? []) as import("../types").McpClientId[];
  const [local, setLocal] = useState<import("../types").McpClientId[]>(propClients);
  const [pending, setPending] = useState(false);
  useEffect(() => {
    setLocal(propClients);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [propClients.join(",")]);

  const isAllOn = local.length === 0;

  async function toggle(c: import("../types").McpClientId) {
    if (pending) return;
    let next: import("../types").McpClientId[];
    if (isAllOn) {
      // Going from "all exposed" → explicit list excluding the clicked one.
      next = ALL_CLIENTS.filter((x) => x !== c);
    } else if (local.includes(c)) {
      const filtered = local.filter((x) => x !== c);
      // If user just turned off the last one, normalize back to "all exposed"
      // — `enabled = false` is the proper way to hide everywhere.
      next = filtered.length === 0 ? [] : filtered;
    } else {
      const added = [...local, c];
      // All 3 checked again → store empty for clean config.toml.
      next = added.length === ALL_CLIENTS.length ? [] : added;
    }
    setLocal(next); // optimistic
    setPending(true);
    try {
      await onChange(next);
    } finally {
      setPending(false);
    }
  }

  const exposedHere = runtime?.exposed_to_current ?? true;
  return (
    <div className={`upstream-clients-row ${pending ? "pending" : ""}`}>
      <span className="smallcaps-tiny">clients</span>
      {ALL_CLIENTS.map((c) => {
        const on = isAllOn || local.includes(c);
        return (
          <button
            key={c}
            type="button"
            className={`client-chip ${on ? "on" : "off"}`}
            onClick={() => toggle(c)}
            disabled={pending}
            title={on ? `exposed to ${c} — click to hide` : `hidden from ${c} — click to expose`}
          >
            {c}
          </button>
        );
      })}
      {pending && <span className="muted">…saving</span>}
      {!pending && !exposedHere && (
        <span
          className="muted"
          title="The current MCP client (this glance-mcp process) is not in the allowlist; this upstream's tools won't appear in tools/list here."
        >
          · hidden from current
        </span>
      )}
    </div>
  );
}

interface UpstreamCardProps {
  entry: UpstreamMcpListEntry;
  onToggle: (next: boolean) => void;
  onClientsChange: (clients: import("../types").McpClientId[]) => Promise<void> | void;
  onRemove: () => void;
  onTest: () => void;
  confirmingRemove: boolean;
  onCancelRemove: () => void;
}

function UpstreamCard({
  entry,
  onToggle,
  onClientsChange,
  onRemove,
  onTest,
  confirmingRemove,
  onCancelRemove,
}: UpstreamCardProps) {
  const { spec, runtime } = entry;
  const status = runtime?.status ?? (spec.enabled ? "failed" : "disabled");
  const dot =
    status === "connected"
      ? "ok"
      : status === "disabled"
      ? "idle"
      : "err";
  const statusLabel =
    status === "connected"
      ? "connected"
      : status === "disabled"
      ? "disabled"
      : "failed";

  const detail =
    spec.type === "stdio"
      ? `${spec.command}${spec.args.length ? " " + spec.args.join(" ") : ""}`
      : spec.url;

  return (
    <div className={`upstream-card ${confirmingRemove ? "confirming" : ""}`}>
      <div className="upstream-card-head">
        <span className={`dot-led ${dot}`} />
        <span className="upstream-card-name">{spec.name}</span>
        <span className="upstream-card-kind">{spec.type === "stdio" ? "stdio" : "http"}</span>
        <span className="upstream-card-status">{statusLabel}</span>
        <span className="upstream-card-tools">
          {runtime?.tool_count ?? 0} tools
          {runtime?.connect_ms != null && (
            <span className="muted"> · {runtime.connect_ms}ms</span>
          )}
        </span>
      </div>

      <div className="upstream-card-detail" title={detail}>
        {detail}
      </div>

      <ClientsRow spec={spec} runtime={runtime ?? null} onChange={onClientsChange} />

      {runtime?.last_error && (
        <div className="upstream-card-err">⚠ {runtime.last_error}</div>
      )}

      <div className="upstream-card-actions">
        <label className="toggle teal">
          <input
            type="checkbox"
            checked={spec.enabled}
            onChange={(e) => onToggle(e.target.checked)}
          />
          <span className="toggle-track" />
        </label>
        <button type="button" className="ping-btn" onClick={onTest}>
          test
        </button>
        {confirmingRemove ? (
          <>
            <button type="button" className="reveal-btn" onClick={onCancelRemove}>
              cancel
            </button>
            <button type="button" className="ping-btn" onClick={onRemove}>
              confirm remove
            </button>
          </>
        ) : (
          <button type="button" className="reveal-btn" onClick={onRemove}>
            remove
          </button>
        )}
      </div>
    </div>
  );
}

interface AddFormProps {
  addMode: AddMode;
  setAddMode: (m: AddMode) => void;
  templates: UpstreamTemplate[];
  draft: UpstreamMcp;
  setDraft: (s: UpstreamMcp) => void;
  draftArgsText: string;
  setDraftArgsText: (s: string) => void;
  draftEnvText: string;
  setDraftEnvText: (s: string) => void;
  setKind: (k: TransportKind) => void;
  applyTemplate: (t: UpstreamTemplate) => void;
  testRes: SmokeTestResult | null;
  testing: boolean;
  saving: boolean;
  onTest: () => void;
  onSave: () => void;
  onCancel: () => void;
}

function AddForm(props: AddFormProps) {
  const {
    addMode,
    setAddMode,
    templates,
    draft,
    setDraft,
    draftArgsText,
    setDraftArgsText,
    draftEnvText,
    setDraftEnvText,
    setKind,
    applyTemplate,
    testRes,
    testing,
    saving,
    onTest,
    onSave,
    onCancel,
  } = props;

  const [pickedSlug, setPickedSlug] = useState("");
  const picked = templates.find((t) => t.slug === pickedSlug) ?? null;

  return (
    <div className="upstream-form">
      <div className="upstream-form-tabs">
        <button
          type="button"
          className={`upstream-form-tab ${addMode === "template" ? "active" : ""}`}
          onClick={() => setAddMode("template")}
        >
          From template
        </button>
        <button
          type="button"
          className={`upstream-form-tab ${addMode === "custom" ? "active" : ""}`}
          onClick={() => setAddMode("custom")}
        >
          Custom
        </button>
        <span className="upstream-form-spacer" />
        <button type="button" className="reveal-btn" onClick={onCancel}>
          cancel
        </button>
      </div>

      {addMode === "template" && (
        <div className="upstream-template-grid">
          <select
            className="inline-input model-select"
            value={pickedSlug}
            onChange={(e) => {
              setPickedSlug(e.target.value);
              const t = templates.find((x) => x.slug === e.target.value);
              if (t) applyTemplate(t);
            }}
          >
            <option value="">— pick a template —</option>
            {templates.map((t) => (
              <option key={t.slug} value={t.slug}>
                {t.label}
              </option>
            ))}
          </select>
          {picked && (
            <p className="muted" style={{ fontSize: 11, marginTop: 2 }}>
              {picked.description}
            </p>
          )}
          {picked && picked.prompts.length > 0 && (
            <div className="upstream-prompts">
              {picked.prompts.map((p) => (
                <div key={p.field} className="muted" style={{ fontSize: 11 }}>
                  ⚠ Required: <b>{p.label}</b>{" "}
                  <span style={{ color: "var(--ink-faint)" }}>({p.field})</span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      <div className="upstream-form-grid">
        <label className="upstream-field">
          <span className="upstream-field-label">name</span>
          <input
            className="inline-input"
            type="text"
            value={draft.name}
            onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            placeholder="context7"
          />
        </label>

        <label className="upstream-field">
          <span className="upstream-field-label">type</span>
          <select
            className="inline-input"
            value={draft.type}
            onChange={(e) => setKind(e.target.value as TransportKind)}
          >
            <option value="stdio">stdio (subprocess)</option>
            <option value="streamable_http">streamable_http (URL)</option>
          </select>
        </label>

        {draft.type === "stdio" ? (
          <>
            <label className="upstream-field">
              <span className="upstream-field-label">command</span>
              <input
                className="inline-input"
                type="text"
                value={draft.command}
                onChange={(e) => setDraft({ ...draft, command: e.target.value })}
                placeholder="npx / context7-mcp / /usr/local/bin/foo"
              />
            </label>
            <label className="upstream-field">
              <span className="upstream-field-label">args</span>
              <input
                className="inline-input"
                type="text"
                value={draftArgsText}
                onChange={(e) => setDraftArgsText(e.target.value)}
                placeholder="@playwright/mcp@latest"
              />
            </label>
            <label className="upstream-field upstream-field-wide">
              <span className="upstream-field-label">env (KEY=value, one per line)</span>
              <textarea
                className="inline-input"
                rows={3}
                value={draftEnvText}
                onChange={(e) => setDraftEnvText(e.target.value)}
                placeholder="NODE_ENV=production"
                style={{ fontFamily: "var(--font-mono)", fontSize: 11 }}
              />
            </label>
          </>
        ) : (
          <>
            <label className="upstream-field upstream-field-wide">
              <span className="upstream-field-label">url</span>
              <input
                className="inline-input"
                type="text"
                value={draft.url}
                onChange={(e) => setDraft({ ...draft, url: e.target.value })}
                placeholder="https://open.bigmodel.cn/api/mcp/zread/mcp"
              />
            </label>
            <label className="upstream-field upstream-field-wide">
              <span className="upstream-field-label">
                api_key{" "}
                <span style={{ color: "var(--ink-faint)" }}>
                  (empty = use backend.api_key)
                </span>
              </span>
              <input
                className="inline-input"
                type="text"
                value={draft.api_key}
                onChange={(e) => setDraft({ ...draft, api_key: e.target.value })}
                placeholder="leave empty to inherit from backend"
              />
            </label>
          </>
        )}
      </div>

      <div className="model-row" style={{ marginTop: 12 }}>
        <button
          type="button"
          className="ping-btn"
          onClick={onTest}
          disabled={testing || !draft.name}
        >
          {testing ? "testing…" : "test before save"}
        </button>
        <button
          type="button"
          className="test-bar-button"
          onClick={onSave}
          disabled={saving || !draft.name}
          style={{ flex: 0, padding: "8px 18px" }}
        >
          <span className="test-bar-num">→</span>
          <span className="test-bar-label">{saving ? "saving…" : "save"}</span>
        </button>
      </div>

      {testRes && (
        <div
          className={`test-bar-result ${testRes.ok ? "ok" : "err"}`}
          style={{ marginTop: 10, fontSize: 11 }}
        >
          {testRes.ok
            ? `✓ ${testRes.name} · ${testRes.tool_count} tools · ${testRes.latency_ms} ms${
                testRes.sample_tools.length
                  ? ` · ${testRes.sample_tools.join(", ")}`
                  : ""
              }`
            : `✗ ${testRes.name || "(unnamed)"} · ${testRes.error || "unknown error"}`}
        </div>
      )}
    </div>
  );
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Naive shell-arg parser: splits on whitespace, honors single + double quotes,
/// no escape handling. Sufficient for the field's expected use (one or two
/// flags). The user can always edit `~/.glance/config.toml` directly for hairy
/// argv.
function parseShellArgs(text: string): string[] {
  const out: string[] = [];
  let cur = "";
  let quote: '"' | "'" | null = null;
  for (const ch of text) {
    if (quote) {
      if (ch === quote) {
        quote = null;
      } else {
        cur += ch;
      }
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch as '"' | "'";
      continue;
    }
    if (/\s/.test(ch)) {
      if (cur.length > 0) {
        out.push(cur);
        cur = "";
      }
      continue;
    }
    cur += ch;
  }
  if (cur.length > 0) out.push(cur);
  return out;
}

function parseEnvText(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const k = line.slice(0, eq).trim();
    const v = line.slice(eq + 1).trim();
    if (k) out[k] = v;
  }
  return out;
}

function envToText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");
}
