import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useConfig } from "./hooks/useConfig";
import { StatusTab } from "./components/StatusTab";
import { BackendTab } from "./components/BackendTab";
import { ToolsTab } from "./components/ToolsTab";
import { LogsTab } from "./components/LogsTab";
import { SettingsTab } from "./components/SettingsTab";
import { UpstreamMcpsTab } from "./components/UpstreamMcpsTab";
import { RtkTab } from "./components/RtkTab";
import { CcusageTab } from "./components/CcusageTab";
import { ChromeTab } from "./components/ChromeTab";

type Tab =
  | "status"
  | "backend"
  | "tools"
  | "logs"
  | "settings"
  | "upstreams"
  | "rtk"
  | "ccusage"
  | "chrome";

interface NavEntry {
  id: Tab;
  num: string;
  cn: string;
  en: string;
}

const NAV: NavEntry[] = [
  { id: "status",    num: "01", cn: "状态",      en: "Status" },
  { id: "backend",   num: "02", cn: "后端",      en: "Backend" },
  { id: "tools",     num: "03", cn: "工具",      en: "Tools" },
  { id: "logs",      num: "04", cn: "日志",      en: "Logs" },
  { id: "settings",  num: "05", cn: "设置",      en: "Settings" },
  { id: "upstreams", num: "06", cn: "上游 MCPs", en: "Upstream MCPs" },
  { id: "rtk",       num: "07", cn: "RTK",        en: "RTK Token Killer" },
  { id: "ccusage",   num: "08", cn: "用量",        en: "CCUSAGE Tracker" },
  { id: "chrome",    num: "09", cn: "Chrome 桥",  en: "Chrome Bridge" },
];

function openExternal(url: string) {
  // Prefer the JS plugin (if available); fall back to the rust command.
  invoke("open_url", { url }).catch(() => {
    // best-effort fallback
    window.open(url, "_blank");
  });
}

export default function App() {
  const [tab, setTab] = useState<Tab>(() => {
    const saved = localStorage.getItem("glance.tab");
    return (saved as Tab) || "status";
  });

  useEffect(() => {
    localStorage.setItem("glance.tab", tab);
  }, [tab]);

  const cfg = useConfig();

  if (cfg.loading) {
    return (
      <div className="loading">
        <div className="loading-mark">glance</div>
        <div className="loading-cap">loading config…</div>
      </div>
    );
  }

  const active = NAV.find((n) => n.id === tab) ?? NAV[0];

  return (
    <div className="app-layout">
      <aside className="sidebar">
        <div className="sidebar-brand">
          <span className="sidebar-aperture" />
          <div className="sidebar-wordmark">
            <span className="sidebar-wordmark-name">glance</span>
            <span className="sidebar-wordmark-sub">MCP · Local</span>
          </div>
        </div>

        <nav className="sidebar-nav">
          {NAV.map((entry) => (
            <button
              key={entry.id}
              type="button"
              className={`nav-item ${tab === entry.id ? "active" : ""}`}
              onClick={() => setTab(entry.id)}
            >
              <span className="nav-item-num">{entry.num}</span>
              <span className="nav-item-labels">
                <span className="nav-item-cn">{entry.cn}</span>
                <span className="nav-item-en">{entry.en}</span>
              </span>
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          <div className="sidebar-footer-about">
            <span className="smallcaps-tiny">About</span>
            <span className="sidebar-footer-version">glance v0.1</span>
          </div>
          <button
            type="button"
            className="sidebar-link"
            onClick={() => openExternal("https://github.com/xtftbwvfp/glance")}
          >
            <span>github / glance</span>
            <span className="sidebar-link-arrow">↗</span>
          </button>
          <button
            type="button"
            className="sidebar-link"
            onClick={() => openExternal("https://github.com/xtftbwvfp/codex-switcher")}
          >
            <span>codex-switcher</span>
            <span className="sidebar-link-arrow">↗</span>
          </button>
        </div>
      </aside>

      <main className="main">
        {tab === "status"    && <StatusTab cfg={cfg} active={active} />}
        {tab === "backend"   && <BackendTab cfg={cfg} active={active} />}
        {tab === "tools"     && <ToolsTab cfg={cfg} active={active} />}
        {tab === "logs"      && <LogsTab active={active} />}
        {tab === "settings"  && <SettingsTab cfg={cfg} active={active} />}
        {tab === "upstreams" && <UpstreamMcpsTab active={active} />}
        {tab === "rtk"       && <RtkTab active={active} />}
        {tab === "ccusage"   && <CcusageTab active={active} />}
        {tab === "chrome"    && <ChromeTab cfg={cfg} active={active} />}
      </main>
    </div>
  );
}

export type { NavEntry };
