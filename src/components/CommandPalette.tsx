import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import * as ipc from "../api/ipc";
import type { FsEntry, GlobalHit, SessionDto } from "../api/types";
import { useApp } from "../state/AppStore";
import { useDebouncedValue } from "../hooks/useDebouncedValue";
import type { Tab } from "./TopBar";
import { renderSnippet } from "./files/SearchResults";
import { IconSearch } from "./files/icons";

interface PaletteItem {
  key: string;
  label: string;
  hint?: string;
  kind?: string;
  snippet?: string;
  run: () => void;
}

const GROUP_CAP = 8;
const SEARCH_DEBOUNCE_MS = 200;

const KIND_LABELS: Record<GlobalHit["kind"], string> = {
  project: "Проект",
  file: "Файл",
  task: "Задача",
  note: "Заметка",
  chat: "Чат",
};

// Case-insensitive scoring: prefix beats substring, substring beats a fuzzy
// subsequence; null means no match.
export function score(text: string, query: string): number | null {
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
  const [hits, setHits] = useState<GlobalHit[] | null>(null);
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

  // Clear results synchronously when the raw (trimmed) query drops below 2
  // chars, so hits disappear on the keystroke instead of after the debounce.
  const trimmedQuery = query.trim();
  useEffect(() => {
    if (trimmedQuery.length < 2) setHits(null);
  }, [trimmedQuery]);

  // Global cross-entity search: debounced, only for queries of 2+ chars; the
  // backend merges projects/files/tasks/notes/chats into one ranked list.
  const debouncedQuery = useDebouncedValue(trimmedQuery, SEARCH_DEBOUNCE_MS);
  useEffect(() => {
    if (debouncedQuery.length < 2) return;
    let cancelled = false;
    void ipc
      .globalSearch(debouncedQuery)
      .then((h) => !cancelled && setHits(h))
      .catch(() => !cancelled && setHits([]));
    return () => {
      cancelled = true;
    };
  }, [debouncedQuery]);

  const q = query.trim().toLowerCase();

  const groups = useMemo(() => {
    // Backend-ranked hits: navigation opens the target entity, setting the
    // matching store intent for the view that mounts with the tab switch.
    const searchItems: PaletteItem[] = (hits ?? []).map((h) => ({
      key: `gs-${h.kind}-${h.id}`,
      label: h.title,
      hint: h.subtitle,
      kind: KIND_LABELS[h.kind] ?? h.kind,
      snippet: h.snippet || undefined,
      run: () => {
        switch (h.kind) {
          case "project":
            void actions.openProject(h.id);
            onTab("projects");
            break;
          case "file":
            if (h.projectId != null && h.relPath) {
              void actions.openProject(h.projectId);
              actions.requestFile(h.relPath);
              onTab("projects");
            }
            break;
          case "task":
            if (h.projectId != null && h.taskId != null) {
              void actions.openProject(h.projectId);
              actions.requestTask(h.taskId);
              onTab("projects");
            }
            break;
          case "note":
            if (h.relPath) {
              actions.requestNote(h.relPath);
              onTab("notes");
            }
            break;
          case "chat":
            if (h.sessionId != null) {
              actions.requestSession(h.sessionId);
              onTab("harness");
            }
            break;
        }
        onClose();
      },
    }));
    // Static groups drop entries the search group already lists, so the same
    // target never appears twice.
    const searchTargets = new Set<string>();
    for (const h of hits ?? []) {
      if (h.kind === "project") searchTargets.add(`project-${h.id}`);
      else if (h.kind === "note" && h.relPath) searchTargets.add(`note-${h.relPath}`);
      else if (h.kind === "chat" && h.sessionId != null) searchTargets.add(`session-${h.sessionId}`);
    }
    const nav: PaletteItem[] = [
      { key: "nav-projects", label: "Проекты", run: () => { onTab("projects"); onClose(); } },
      { key: "nav-harness", label: "Харнес", run: () => { onTab("harness"); onClose(); } },
      { key: "nav-notes", label: "Блокнот", run: () => { onTab("notes"); onClose(); } },
      { key: "nav-settings", label: "Настройки", run: () => { actions.requestSettings(); onClose(); } },
    ];
    const projectItems: PaletteItem[] = state.projects
      .filter((p) => !searchTargets.has(`project-${p.id}`))
      .map((p) => ({
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
      .filter((e) => e.kind === "file" && !searchTargets.has(`note-${e.relPath}`))
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
    const sessionItems: PaletteItem[] = (sessions ?? [])
      .filter((s) => !searchTargets.has(`session-${s.id}`))
      .map((s) => ({
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
    // The Поиск group is already ranked by the backend: no local rescoring.
    const defs: Array<[string, PaletteItem[], boolean]> = [
      ["Поиск", searchItems, true],
      ["Перейти", nav, false],
      ["Проекты", projectItems, false],
      ["Заметки", noteItems, false],
      ["Чаты харнеса", sessionItems, false],
      ["Действия", actionItems, false],
    ];
    return defs
      .map(([title, items, preRanked]) => {
        if (!q || preRanked) return { title, items: items.slice(0, GROUP_CAP) };
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
  }, [q, hits, state.projects, notes, sessions, actions, onTab, onClose]);

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
            placeholder="Проекты, файлы, задачи, заметки, чаты, действия…"
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
                    {it.kind && <span className="palette-item-kind">{it.kind}</span>}
                    <span className="palette-item-name">{it.label}</span>
                    {it.hint && <span className="palette-item-hint">{it.hint}</span>}
                    {it.snippet && (
                      <span className="palette-item-snippet">{renderSnippet(it.snippet)}</span>
                    )}
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
