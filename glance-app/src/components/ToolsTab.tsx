import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ToolEntry } from "../types";
import type { NavEntry } from "../App";
import { TabHeader } from "./TabHeader";

interface Props {
  cfg: ReturnType<typeof import("../hooks/useConfig").useConfig>;
  active: NavEntry;
}

const TOOL_DESC: Record<string, string> = {
  research:           "Pull multiple files / links · synthesize a brief.",
  explain:            "Explain any snippet or repository corner.",
  search:             "Single-shot web or in-repo search.",
  md_read:            "Read a Markdown file (full or by section).",
  md_outline:         "Extract Markdown outline (heading tree).",
  obsidian_read:      "Read Obsidian vault notes by path or alias.",
  obsidian_search:    "Full-text search over the Obsidian vault.",
  obsidian_backlinks: "List backlinks pointing into a note.",
  write_tests:        "Author tests · emits a patch file.",
  write_docs:         "Write or update documentation · patch.",
  fix_lint:           "Tidy lint / formatting · patch.",
  md_write:           "Rewrite a Markdown file · patch.",
  obsidian_write:     "Edit an Obsidian note · patch.",
};

const ALL_CLIENTS = ["claude", "codex", "cursor"] as const;
type Client = (typeof ALL_CLIENTS)[number];

export function ToolsTab({ cfg, active }: Props) {
  const [list, setList] = useState<ToolEntry[]>([]);
  // Per-tool client allowlist. Missing key OR all 3 = "any client".
  const [toolClients, setToolClients] = useState<Record<string, string[]>>({});

  useEffect(() => {
    invoke<ToolEntry[]>("list_tool_toggles").then(setList).catch(() => {});
    invoke<{ key: string; clients: string[] }[]>("list_tool_clients")
      .then((rows) => {
        const map: Record<string, string[]> = {};
        for (const r of rows) map[r.key] = r.clients;
        setToolClients(map);
      })
      .catch(() => {});
  }, []);

  if (!cfg.cfg) return <div className="muted">Loading…</div>;
  const c = cfg.cfg;
  const tools: Record<string, boolean> = c.tools as Record<string, boolean>;

  function toggle(key: string) {
    cfg.patch(
      (p) => ({
        ...p,
        tools: { ...p.tools, [key]: !(p.tools as Record<string, boolean>)[key] },
      }),
      { save: true },
    );
  }

  // True iff this client currently sees this tool. Empty list / missing key
  // means "any" — visible to all clients.
  function isAllowed(key: string, client: Client) {
    const list = toolClients[key];
    if (!list || list.length === 0 || list.length === ALL_CLIENTS.length) return true;
    return list.includes(client);
  }

  async function toggleClient(key: string, client: Client) {
    const cur = toolClients[key] || [];
    // Treat empty / full as the "all enabled" baseline.
    const allEnabled = cur.length === 0 || cur.length === ALL_CLIENTS.length;
    let next: string[];
    if (allEnabled) {
      // Switching from "all" to "all-but-one" → drop the clicked client.
      next = ALL_CLIENTS.filter((c) => c !== client);
    } else if (cur.includes(client)) {
      next = cur.filter((c) => c !== client);
    } else {
      next = [...cur, client];
    }
    setToolClients((prev) => ({ ...prev, [key]: next }));
    try {
      await invoke("set_tool_clients", { args: { name: key, clients: next } });
    } catch (e) {
      console.error("set_tool_clients failed", e);
    }
  }

  const readTools = list.filter((t) => t.category === "read");
  const writeTools = list.filter((t) => t.category === "write");
  const readOn = readTools.filter((t) => tools[t.key]).length;
  const writeOn = writeTools.filter((t) => tools[t.key]).length;

  function renderRow(t: ToolEntry, isWrite: boolean) {
    const checked = !!tools[t.key];
    return (
      <div className="tool-row" key={t.key}>
        <div className="tool-row-info">
          <span className="tool-row-name">{t.key}</span>
          <span className="tool-row-desc">{TOOL_DESC[t.key] || ""}</span>
        </div>
        <div className="tool-row-clients" title="Limit which MCP clients see this tool. All on = visible everywhere.">
          {ALL_CLIENTS.map((cli) => {
            const on = isAllowed(t.key, cli);
            return (
              <button
                key={cli}
                type="button"
                className={`client-pill ${on ? "on" : "off"}`}
                disabled={!checked}
                onClick={() => toggleClient(t.key, cli)}
              >
                {cli}
              </button>
            );
          })}
        </div>
        <label className={`toggle ${isWrite ? "" : "teal"}`}>
          <input type="checkbox" checked={checked} onChange={() => toggle(t.key)} />
          <span className="toggle-track" />
        </label>
      </div>
    );
  }

  const meta = (
    <>
      <span>
        EXPOSED <b>{readOn + writeOn}</b> / {list.length}
      </span>
      <span>
        STATE <b>{cfg.saving ? "saving…" : "synced"}</b>
      </span>
    </>
  );

  return (
    <div className="tab-page">
      <TabHeader active={active} meta={meta} />

      <div className="tab-body">
        <section className="tool-group">
          <header className="tool-group-header">
            <span className="tool-group-title">
              Read-only · {readTools.length} tools
            </span>
            <span className="tool-group-count">
              {readOn}<span style={{ color: "var(--ink-faint)" }}> / {readTools.length}</span>
            </span>
          </header>
          {readTools.map((t) => renderRow(t, false))}
        </section>

        <section className="tool-group">
          <header className="tool-group-header">
            <span className="tool-group-title">
              Write / patch mode · {writeTools.length} tools
            </span>
            <span className="tool-group-count">
              {writeOn}<span style={{ color: "var(--ink-faint)" }}> / {writeTools.length}</span>
            </span>
          </header>
          <div className="tool-group-banner">
            Write tools emit patch files to <b>~/.glance/patches/</b> — they never touch
            your disk directly. Subject to <b>deny_paths</b> and <b>deny_keywords</b>.
          </div>
          {writeTools.map((t) => renderRow(t, true))}
        </section>
      </div>
    </div>
  );
}
