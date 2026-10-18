import { useEffect, useState, type MouseEvent } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import logo from "../assets/logo.svg";
import * as ipc from "../api/ipc";
import type { ReindexStatus } from "../api/types";
import { IconSearch } from "./files/icons";
import { toast } from "./shared/toast";

export type Tab = "today" | "projects" | "harness" | "notes";

interface Props {
  tab: Tab;
  onTab: (tab: Tab) => void;
  onSettings: () => void;
  onSearch: () => void;
}

function fmtCountdown(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000));
  const mm = Math.floor(total / 60);
  const ss = total % 60;
  return `${mm}:${String(ss).padStart(2, "0")}`;
}

function IconRefresh() {
  return (
    <svg
      viewBox="0 0 24 24"
      width="15"
      height="15"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M21 12a9 9 0 1 1-2.64-6.36" />
      <path d="M21 3v6h-6" />
    </svg>
  );
}

export default function TopBar({ tab, onTab, onSettings, onSearch }: Props) {
  // «Переиндексация поиска»: idle shows the auto-reindex countdown (mm:ss),
  // a running pass shows done/total; click starts a pass right away.
  const [reindex, setReindex] = useState<ReindexStatus | null>(null);
  const [now, setNow] = useState(() => Date.now());

  // Status poll: every 1 s while running, every 5 s idle.
  useEffect(() => {
    let alive = true;
    const tick = () => {
      setNow(Date.now());
      ipc
        .reindexStatus()
        .then((s) => alive && setReindex(s))
        .catch(() => {});
    };
    tick();
    const timer = setInterval(tick, reindex?.running ? 1000 : 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [reindex?.running]);

  // Backend events: progress updates the counter right away, done toasts and
  // refetches the status (the countdown resets to 5:00 with the new nextRunAt).
  useEffect(() => {
    let alive = true;
    const unlisten: Array<() => void> = [];
    const track = (p: Promise<() => void>) =>
      p.then((u) => {
        if (alive) unlisten.push(u);
        else u();
      });
    track(
      ipc.onReindexProgress((p) =>
        alive &&
        setReindex((prev) =>
          prev ? { ...prev, running: true, done: p.done, total: p.total, phase: p.phase } : prev,
        ),
      ),
    );
    track(
      ipc.onReindexDone(() => {
        if (!alive) return;
        toast("ok", "Поиск переиндексирован");
        ipc
          .reindexStatus()
          .then((s) => alive && setReindex(s))
          .catch(() => {});
      }),
    );
    return () => {
      alive = false;
      unlisten.forEach((u) => u());
    };
  }, []);

  const startReindex = () => {
    ipc
      .reindexStart()
      .then(setReindex)
      .catch(() => toast("err", "Не удалось запустить переиндексацию"));
  };

  const running = reindex?.running ?? false;
  const label = running
    ? `${reindex?.done ?? 0}/${reindex?.total ?? 0}`
    : reindex
      ? fmtCountdown(reindex.nextRunAt - now)
      : "";
  const title =
    running && reindex?.phase ? `Переиндексация поиска: ${reindex.phase}` : "Переиндексация поиска";

  // Explicit window drag: WKWebView does not reliably honor
  // data-tauri-drag-region / -webkit-app-region, so start the native drag
  // ourselves on a plain left-click over the bar. Interactive controls and
  // the tab strip keep their own behavior.
  const onDragMouseDown = (e: MouseEvent) => {
    if (e.button !== 0) return;
    const el = e.target as HTMLElement;
    if (el.closest("button, input, select, textarea, a, .tabs, .icon-btn")) return;
    e.preventDefault();
    getCurrentWindow().startDragging().catch(() => {});
  };

  return (
    <header className="topbar" data-tauri-drag-region onMouseDown={onDragMouseDown}>
      <div className="topbar-left" data-tauri-drag-region onMouseDown={onDragMouseDown}>
        <img className="app-logo" src={logo} alt="" data-tauri-drag-region onMouseDown={onDragMouseDown} />
      </div>
      <nav className="tabs">
        <button
          className={tab === "today" ? "tab active" : "tab"}
          onClick={() => onTab("today")}
        >
          Сегодня
        </button>
        <button
          className={tab === "projects" ? "tab active" : "tab"}
          onClick={() => onTab("projects")}
        >
          Проекты
        </button>
        <button
          className={tab === "harness" ? "tab active" : "tab"}
          onClick={() => onTab("harness")}
        >
          Харнес
        </button>
        <button
          className={tab === "notes" ? "tab active" : "tab"}
          onClick={() => onTab("notes")}
        >
          Блокнот
        </button>
      </nav>
      <div className="topbar-right" data-tauri-drag-region onMouseDown={onDragMouseDown}>
        <button className="icon-btn" title="Поиск (⌘K)" onClick={onSearch}>
          <IconSearch />
        </button>
        <button className="icon-btn reindex-btn" title={title} onClick={startReindex}>
          <IconRefresh />
          <span className="reindex-count">{label}</span>
        </button>
        <button className="icon-btn" title="Настройки" onClick={onSettings}>
          <svg
            viewBox="0 0 24 24"
            width="17"
            height="17"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h.01a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h.01a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v.01a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
          </svg>
        </button>
      </div>
    </header>
  );
}
