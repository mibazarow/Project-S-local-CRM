import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import * as ipc from "../api/ipc";
import type { FsEntry, SessionDto } from "../api/types";
import { useApp } from "../state/AppStore";
import type { Tab } from "./TopBar";
import { IconSearch } from "./files/icons";

interface PaletteItem {
  key: string;
  label: string;
  hint?: string;
  run: () => void;
}

const GROUP_CAP = 8;

// Case-insensitive scoring: prefix beats substring, substring beats a fuzzy
// subsequence; null means no match.
function score(text: string, query: string): number | null {
  const t = text.toLowerCase();
  const at = t.indexOf(query);
  if (at === 0) return 100;
  if (at > 0) return 60 - Math.min(at, 30);
  let cursor = 0;
  let gaps = 0;
  for (const ch of query) {
    const found = t.indexOf(ch, cursor);
    if (found === -1) return null;
    gaps += found - cursor;
    cursor = found + 1;
  }
  return 30 - Math.min(gaps, 25);
}

interface Props {
  onTab: (tab: Tab) => void;
  onClose: () => void;
}

export default function CommandPalette({ onTab, onClose }: Props) {
  const { state, actions } = useApp();
  const [query, setQuery] = useState("");
  const [notes, setNotes] = useState<FsEntry[] | null>(null);
  const [sessions, setSessions] = useState<SessionDto[] | null>(null);
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    void ipc
      .notesTree()
      .then((n) => !cancelled && setNotes(n))
      .catch(() => !cancelled && setNotes([]));
    void ipc
      .harnessListSessions()
      .then((s) => !cancelled && setSessions(s))
      .catch(() => !cancelled && setSessions([]));
    return () => {
      cancelled = true;
    };
  }, []);

  const q = query.trim().toLowerCase();

  const groups = useMemo(() => {
    const nav: PaletteItem[] = [
      { key: "nav-projects", label: "Проекты", run: () => { onTab("projects"); onClose(); } },
      { key: "nav-harness", label: "Харнес", run: () => { onTab("harness"); onClose(); } },
      { key: "nav-journal", label: "Журнал", run: () => { onTab("journal"); onClose(); } },
      { key: "nav-notes", label: "Блокнот", run: () => { onTab("notes"); onClose(); } },
      { key: "nav-settings", label: "Настройки", run: () => { actions.requestSettings(); onClose(); } },
    ];
    const projectItems: PaletteItem[] = state.projects.map((p) => ({
      key: `project-${p.id}`,
      label: p.name,
      hint: `файлов: ${p.fileCount}`,
      run: () => {
        void actions.openProject(p.id);
        onTab("projects");
        onClose();
      },
    }));
    const noteItems: PaletteItem[] = (notes ?? [])
      .filter((e) => e.kind === "file")
      .map((e) => ({
        key: `note-${e.relPath}`,
        label: e.name.replace(/\.(md|markdown)$/i, ""),
        hint: e.relPath,
        run: () => {
          actions.requestNote(e.relPath);
          onTab("notes");
          onClose();
        },
      }));
    const sessionItems: PaletteItem[] = (sessions ?? []).map((s) => ({
      key: `session-${s.id}`,
      label: s.title ?? "Новая сессия",
      run: () => {
        actions.requestSession(s.id);
        onTab("harness");
        onClose();
      },
    }));
    const actionItems: PaletteItem[] = [
      {
        key: "act-new-project",
        label: "Новый проект",
        hint: "форма в «Проектах»",
        run: () => {
          actions.requestNewProject();
          onTab("projects");
          onClose();
        },
      },
      {
        key: "act-new-note",
        label: "Новая заметка",
        hint: "форма в «Блокноте»",
        run: () => {
          actions.requestNewNote();
          onTab("notes");
          onClose();
        },
      },
      {
        key: "act-new-session",
        label: "Новый чат харнеса",
        hint: "создать и открыть",
        run: () => {
          void ipc
            .harnessCreateSession()
            .then((dto) => {
              actions.requestSession(dto.id);
              onTab("harness");
            })
            .catch(() => {})
            .finally(onClose);
        },
      },
      {
        key: "act-reindex",
        label: "Переиндексировать всё",
        hint: "поиск по проектам",
        run: () => {
          void ipc.reindexAll().catch(() => {});
          onClose();
        },
      },
    ];
    const defs: Array<[string, PaletteItem[]]> = [
      ["Перейти", nav],
      ["Проекты", projectItems],
      ["Заметки", noteItems],
      ["Чаты харнеса", sessionItems],
      ["Действия", actionItems],
    ];
    return defs
      .map(([title, items]) => {
        if (!q) return { title, items: items.slice(0, GROUP_CAP) };
        return {
          title,
          items: items
            .map((it) => ({ it, s: score(`${it.label} ${it.hint ?? ""}`, q) }))
            .filter((x): x is { it: PaletteItem; s: number } => x.s !== null)
            .sort((a, b) => b.s - a.s)
            .slice(0, GROUP_CAP)
            .map((x) => x.it),
        };
      })
      .filter((g) => g.items.length > 0);
  }, [q, state.projects, notes, sessions, actions, onTab, onClose]);

  const flat = useMemo(() => groups.flatMap((g) => g.items), [groups]);

  useEffect(() => {
    setActive(0);
  }, [q]);

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>(`[data-idx="${active}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [active, groups]);

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      onClose();
      return;
    }
    if (e.key === "ArrowDown" && flat.length > 0) {
      e.preventDefault();
      setActive((i) => (i + 1) % flat.length);
      return;
    }
    if (e.key === "ArrowUp" && flat.length > 0) {
      e.preventDefault();
      setActive((i) => (i - 1 + flat.length) % flat.length);
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      flat[active]?.run();
    }
  };

  let idx = -1;

  return (
    <div
      className="palette-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="palette">
        <div className="palette-search">
          <IconSearch />
          <input
            autoFocus
            type="text"
            placeholder="Проекты, заметки, чаты, действия…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </div>
        <div className="palette-list" ref={listRef}>
          {groups.map((g) => (
            <div className="palette-group" key={g.title}>
              <div className="palette-group-title">{g.title}</div>
              {g.items.map((it) => {
                idx += 1;
                const myIdx = idx;
                return (
                  <div
                    key={it.key}
                    data-idx={myIdx}
                    className={myIdx === active ? "palette-item active" : "palette-item"}
                    role="button"
                    tabIndex={-1}
                    onClick={() => it.run()}
                    onMouseEnter={() => setActive(myIdx)}
                  >
                    <span className="palette-item-name">{it.label}</span>
                    {it.hint && <span className="palette-item-hint">{it.hint}</span>}
                  </div>
                );
              })}
            </div>
          ))}
          {flat.length === 0 && <div className="empty-note">Ничего не найдено</div>}
        </div>
        <div className="palette-foot">
          <span>↑↓ — выбор</span>
          <span>Enter — открыть</span>
          <span>Esc — закрыть</span>
        </div>
      </div>
    </div>
  );
}
