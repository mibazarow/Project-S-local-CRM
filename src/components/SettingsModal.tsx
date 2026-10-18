import { useEffect, useState } from "react";
import * as ipc from "../api/ipc";
import type { AppConfig } from "../api/types";
import { useDebouncedConfig } from "./settings/useDebouncedConfig";
import { appToPatch, toAppForm, type AppForm } from "./settings/forms";
import LlmTab from "./settings/LlmTab";
import AgentTab from "./settings/AgentTab";
import SkillsTab from "./settings/SkillsTab";
import McpTab from "./settings/McpTab";
import OtherTab from "./settings/OtherTab";

type TabId = "llm" | "agent" | "skills" | "mcp" | "other";

// Modal shell only: sidebar, open/close, backdrop, active tab. Each tab lives
// in its own component under settings/ and lazy-mounts on first open; opened
// tabs stay mounted (rendering null while inactive), so switching tabs never
// re-reads their data or resets in-flight form state, exactly as before.
export default function SettingsModal({ onClose }: { onClose: () => void }) {
  const [tab, setTab] = useState<TabId>("llm");
  const [opened, setOpened] = useState<Record<TabId, boolean>>({
    llm: true,
    agent: false,
    skills: false,
    mcp: false,
    other: false,
  });
  // One error line shared by the tabs that show it (llm/agent/other).
  const [error, setError] = useState<string | null>(null);
  // app.json fields edited by the "Агент" and "Прочее" tabs share one form so
  // a single debounced writer owns the file.
  const app = useDebouncedConfig<AppForm>(
    "app",
    (raw) => toAppForm(raw as Partial<AppConfig>),
    appToPatch,
    setError,
  );
  const { reload: reloadAppConfig } = app;

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void ipc
      .onConfigChanged(({ which }) => {
        if (which === "app") reloadAppConfig();
      })
      .then((u) => (cancelled ? u() : (unlisten = u)));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reloadAppConfig]);

  const openTab = (next: TabId) => {
    setTab(next);
    setOpened((o) => (o[next] ? o : { ...o, [next]: true }));
  };

  return (
    <div
      className="modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal settings-modal">
        <nav className="settings-nav">
          <div className="settings-title">Настройки</div>
          <button
            className={tab === "llm" ? "settings-tab active" : "settings-tab"}
            onClick={() => openTab("llm")}
          >
            Модель
          </button>
          <button
            className={tab === "agent" ? "settings-tab active" : "settings-tab"}
            onClick={() => openTab("agent")}
          >
            Агент
          </button>
          <button
            className={tab === "skills" ? "settings-tab active" : "settings-tab"}
            onClick={() => openTab("skills")}
          >
            Скиллы
          </button>
          <button
            className={tab === "mcp" ? "settings-tab active" : "settings-tab"}
            onClick={() => openTab("mcp")}
          >
            MCP
          </button>
          <button
            className={tab === "other" ? "settings-tab active" : "settings-tab"}
            onClick={() => openTab("other")}
          >
            Прочее
          </button>
        </nav>
        <div className="settings-body">
          <button className="icon-btn small settings-close" title="Закрыть" onClick={onClose}>
            <svg
              viewBox="0 0 24 24"
              width="15"
              height="15"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
            >
              <path d="M18 6 6 18M6 6l12 12" />
            </svg>
          </button>
          <LlmTab active={tab === "llm"} error={error} setError={setError} />
          {opened.agent && (
            <AgentTab active={tab === "agent"} error={error} setError={setError} app={app} />
          )}
          {opened.skills && <SkillsTab active={tab === "skills"} />}
          {opened.mcp && <McpTab active={tab === "mcp"} />}
          {opened.other && (
            <OtherTab active={tab === "other"} error={error} setError={setError} app={app} />
          )}
        </div>
      </div>
    </div>
  );
}
