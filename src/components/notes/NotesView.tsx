import { useCallback, useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import NoteEditor from "./NoteEditor";
import NotesChat from "./NotesChat";
import NotesSearchResults from "./NotesSearchResults";
import NotesTree from "./NotesTree";
import { IconClose, IconSearch } from "../files/icons";

// NOTES_SCOPE_ID in index/mod.rs: "index-changed" rows for the Notes/ vault
// (agent tool writes reach the tree through the debounced reindex watcher).
const NOTES_SCOPE_ID = -1;

export default function NotesView() {
  const [treeVersion, setTreeVersion] = useState(0);
  const [selected, setSelected] = useState<FsEntry | null>(null);
  const [query, setQuery] = useState("");
  const [activeQuery, setActiveQuery] = useState("");
  const dirtyRef = useRef(false);
  const setDirty = useCallback((dirty: boolean) => {
    dirtyRef.current = dirty;
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => setActiveQuery(query.trim()), 250);
    return () => clearTimeout(timer);
  }, [query]);

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
            onSelect={handleSelect}
            onDeleted={handleDeleted}
            onRenamed={handleRenamed}
          />
        )}
      </aside>

      <NoteEditor entry={selected} onDirtyChange={setDirty} />

      <NotesChat />
    </main>
  );
}
