import { useCallback, useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { useApp } from "../../state/AppStore";
import { useDebouncedValue } from "../../hooks/useDebouncedValue";
import NoteEditor from "./NoteEditor";
import NotesChat from "./NotesChat";
import NotesSearchResults from "./NotesSearchResults";
import NotesTree from "./NotesTree";
import { IconClose, IconSearch } from "../files/icons";

// NOTES_SCOPE_ID in index/mod.rs: "index-changed" rows for the Notes/ vault
// (agent tool writes reach the tree through the debounced reindex watcher).
const NOTES_SCOPE_ID = -1;

export default function NotesView() {
  const { state, actions } = useApp();
  const [treeVersion, setTreeVersion] = useState(0);
  const [selected, setSelected] = useState<FsEntry | null>(null);
  const [query, setQuery] = useState("");
  const activeQuery = useDebouncedValue(query.trim(), 250);
  const [createTick, setCreateTick] = useState(0);
  const dirtyRef = useRef(false);
  const setDirty = useCallback((dirty: boolean) => {
    dirtyRef.current = dirty;
  }, []);

  // Seed Notes/ and the welcome note on first open, then load the tree.
  useEffect(() => {
    let cancelled = false;
    void ipc
      .notesEnsure()
      .then(() => !cancelled && setTreeVersion((v) => v + 1))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unNotes: (() => void) | undefined;
    let unIndex: (() => void) | undefined;
    const bump = () => setTreeVersion((v) => v + 1);
    void ipc
      .onNotesChanged(() => bump())
      .then((u) => (cancelled ? u() : (unNotes = u)));
    void ipc
      .onIndexChanged((p) => {
        if (p.projectId === NOTES_SCOPE_ID) bump();
      })
      .then((u) => (cancelled ? u() : (unIndex = u)));
    return () => {
      cancelled = true;
      unNotes?.();
      unIndex?.();
    };
  }, []);

  const confirmDiscard = () =>
    !dirtyRef.current ||
    window.confirm("Есть несохранённые изменения. Отменить их?");

  const handleSelect = (entry: FsEntry) => {
    if (entry.relPath !== selected?.relPath && !confirmDiscard()) return;
    setSelected(entry);
  };

  const handleDeleted = (rel: string) => {
    if (selected && (selected.relPath === rel || selected.relPath.startsWith(`${rel}/`))) {
      setSelected(null);
    }
  };

  const handleRenamed = (oldRel: string, entry: FsEntry) => {
    if (!selected) return;
    if (selected.relPath === oldRel) setSelected(entry);
    else if (selected.relPath.startsWith(`${oldRel}/`)) setSelected(null);
  };

  // After a DnD move the open note keeps its content but gets a new path.
  const handleMoved = (oldRel: string, entry: FsEntry) => {
    if (!selected) return;
    if (selected.relPath === oldRel) setSelected(entry);
    else if (selected.relPath.startsWith(`${oldRel}/`)) {
      setSelected({ ...selected, relPath: `${entry.relPath}/${selected.relPath.slice(oldRel.length + 1)}` });
    }
  };

  // Command-palette intent: open a note by vault-relative path (resolved to a
  // real FsEntry through the folder listing, like the search results do).
  useEffect(() => {
    const path = state.pendingNotePath;
    if (!path) return;
    actions.clearPendingNote();
    if (!confirmDiscard()) return;
    void (async () => {
      const slash = path.lastIndexOf("/");
      const dir = slash === -1 ? "" : path.slice(0, slash);
      try {
        const entries = await ipc.notesList(dir);
        const entry = entries.find((e) => e.relPath === path);
        if (entry) setSelected(entry);
      } catch {
        // The note vanished between palette load and open: stay put.
      }
    })();
  }, [state.pendingNotePath, actions]);

  // Command-palette intent: focus the tree's "Новая заметка" create input.
  useEffect(() => {
    if (!state.newNoteRequested) return;
    actions.clearNewNote();
    setCreateTick((v) => v + 1);
  }, [state.newNoteRequested, actions]);

  return (
    <main className="view active notes-view">
      <aside className="sidebar">
        <div className="sidebar-head">
          <span className="sidebar-title">Заметки</span>
        </div>
        <div className="search notes-search">
          <IconSearch />
          <input
            type="text"
            placeholder="Поиск по заметкам…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          {query && (
            <button
              className="icon-btn small"
              title="Очистить поиск"
              onClick={() => setQuery("")}
            >
              <IconClose />
            </button>
          )}
        </div>
        {activeQuery ? (
          <NotesSearchResults
            query={activeQuery}
            treeVersion={treeVersion}
            onOpen={handleSelect}
          />
        ) : (
          <NotesTree
            treeVersion={treeVersion}
            selectedRelPath={selected?.relPath ?? null}
            selectedDirRel={selected && selected.kind === "dir" ? selected.relPath : ""}
            createTick={createTick}
            onSelect={handleSelect}
            onDeleted={handleDeleted}
            onRenamed={handleRenamed}
            onMoved={handleMoved}
          />
        )}
      </aside>

      <NoteEditor entry={selected} onDirtyChange={setDirty} />

      <NotesChat />
    </main>
  );
}
