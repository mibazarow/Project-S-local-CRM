import { useEffect, useState, type PointerEvent as ReactPointerEvent } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, TemplateSummary } from "../../api/types";
import { confirmRemove } from "../../shared/confirm";
import { usePointerDrag } from "../../hooks/usePointerDrag";
import { toMessage } from "../chat/chatShared";
import RenameInput from "../files/RenameInput";
import {
  EntryIcon,
  IconChevron,
  IconFilePlus,
  IconFolderPlus,
  IconPencil,
  IconTrash,
} from "../files/icons";

// New folders are seeded with this note: notes_create only creates files
// (parents on demand), so an empty folder cannot exist without a note.
const FOLDER_SEED_NOTE = "Новая заметка.md";

interface Props {
  treeVersion: number;
  selectedRelPath: string | null;
  selectedDirRel: string;
  // Bumped by the command palette's «Новая заметка» action: opens the inline
  // create input for a file under the selected folder.
  createTick?: number;
  onSelect: (entry: FsEntry) => void;
  onDeleted: (rel: string) => void;
  onRenamed: (oldRel: string, entry: FsEntry) => void;
  onMoved: (oldRel: string, entry: FsEntry) => void;
}

function joinRel(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

// Notes are markdown: a name typed without an extension gets ".md".
function noteFileName(name: string): string {
  return name.includes(".") ? name : `${name}.md`;
}

function NotesRow({
  entry,
  open,
  selected,
  renaming,
  dragging,
  dropOver,
  noDrop,
  onToggle,
  onSelect,
  onStartRename,
  onRename,
  onRenameCancel,
  onDelete,
  onPointerDown,
  consumeSuppressedClick,
}: {
  entry: FsEntry;
  open: boolean;
  selected: boolean;
  renaming: boolean;
  dragging: boolean;
  dropOver: boolean;
  noDrop: boolean;
  onToggle: (entry: FsEntry) => void;
  onSelect: (entry: FsEntry) => void;
  onStartRename: (entry: FsEntry) => void;
  onRename: (entry: FsEntry, name: string) => void;
  onRenameCancel: () => void;
  onDelete: (entry: FsEntry) => void;
  onPointerDown: (entry: FsEntry, e: ReactPointerEvent<HTMLDivElement>) => void;
  consumeSuppressedClick: () => boolean;
}) {
  const isDir = entry.kind === "dir";
  const classes = [
    "tree-row",
    isDir && open ? "open" : "",
    selected ? "selected" : "",
    dragging ? "dragging" : "",
    dropOver ? "drop-over" : "",
    noDrop ? "no-drop" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div
      className={classes}
      data-drop-dir={isDir ? entry.relPath : undefined}
      onPointerDown={(e) => {
        if (!renaming) onPointerDown(entry, e);
      }}
      onClick={() => {
        // A drag must not open/toggle the row it ended on.
        if (consumeSuppressedClick()) return;
        if (isDir) onToggle(entry);
        onSelect(entry);
      }}
    >
      {isDir && (
        <span className="chev">
          <IconChevron />
        </span>
      )}
      <span className={isDir ? "icon folder-icon" : "icon"}>
        <EntryIcon entry={entry} />
      </span>
      {renaming ? (
        <RenameInput
          value={entry.name}
          selectBase={!isDir}
          onCommit={(name) => onRename(entry, name)}
          onCancel={onRenameCancel}
        />
      ) : (
        <span className={isDir ? "label folder-label" : "label"}>{entry.name}</span>
      )}
      <span className="row-actions">
        <button
          className="mini-btn"
          title="Переименовать"
          onClick={(e) => {
            e.stopPropagation();
            onStartRename(entry);
          }}
        >
          <IconPencil />
        </button>
        <button
          className="mini-btn danger"
          title="Удалить"
          onClick={(e) => {
            e.stopPropagation();
            onDelete(entry);
          }}
        >
          <IconTrash />
        </button>
      </span>
      {entry.ext !== "" && <span className="tag">{entry.ext}</span>}
    </div>
  );
}

export default function NotesTree({
  treeVersion,
  selectedRelPath,
  selectedDirRel,
  createTick,
  onSelect,
  onDeleted,
  onRenamed,
  onMoved,
}: Props) {
  const [childrenByDir, setChildrenByDir] = useState<Record<string, FsEntry[]>>({});
  const [openPaths, setOpenPaths] = useState<Record<string, boolean>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [renamingPath, setRenamingPath] = useState<string | null>(null);
  const [creating, setCreating] = useState<{ parentRel: string; kind: "dir" | "file" } | null>(
    null,
  );
  const [noteTemplates, setNoteTemplates] = useState<TemplateSummary[]>([]);

  // Note templates for the «Из шаблона» picker; a failed load just hides it.
  useEffect(() => {
    let cancelled = false;
    void ipc
      .templatesList()
      .then((t) => !cancelled && setNoteTemplates(t.notes))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  const handleTemplate = async (templateName: string) => {
    if (!templateName) return;
    const custom = window.prompt("Имя новой заметки (можно оставить имя шаблона):", templateName);
    if (custom === null) return;
    const name = custom.trim();
    try {
      const created = await ipc.templateCreateNote(
        templateName,
        name && name !== templateName ? name : undefined,
      );
      onSelect(created);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  // notes_tree returns the whole vault as one flat list; group it by parent
  // dir (backend order is already dirs-first case-insensitive).
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    void (async () => {
      try {
        const entries = await ipc.notesTree();
        if (cancelled) return;
        const next: Record<string, FsEntry[]> = {};
        for (const e of entries) {
          const slash = e.relPath.lastIndexOf("/");
          const dir = slash === -1 ? "" : e.relPath.slice(0, slash);
          (next[dir] ??= []).push(e);
        }
        setChildrenByDir(next);
        setError(null);
      } catch (e) {
        if (!cancelled) setError(toMessage(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [treeVersion]);

  const toggle = (entry: FsEntry) =>
    setOpenPaths((prev) => {
      const next = { ...prev };
      if (next[entry.relPath]) delete next[entry.relPath];
      else next[entry.relPath] = true;
      return next;
    });

  // Backend also rejects these moves; the UI guard only prevents the attempt.
  const canDrop = (src: string, dest: string) =>
    dest !== src && !dest.startsWith(`${src}/`);

  const commitMove = async (fromRel: string, toDirRel: string) => {
    try {
      const moved = await ipc.notesMove(fromRel, toDirRel);
      onMoved(fromRel, moved);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  // Pointer DnD (HTML5 DnD is unreliable in WKWebView): the drop target is the
  // folder row under the pointer (data-drop-dir) or the tree root for "".
  const { draggingId, dropTarget, onPointerDown, consumeSuppressedClick } = usePointerDrag<
    string
  >({
    resolveTarget: (clientX, clientY, id) => {
      const el = document
        .elementFromPoint(clientX, clientY)
        ?.closest<HTMLElement>("[data-drop-dir]");
      const container = el?.dataset.dropDir;
      if (container === undefined) return null;
      return { container, index: 0, draggingId: id };
    },
    onDrop: (id, target) => {
      const dest = target.container;
      if (!canDrop(id, dest)) return;
      const slash = id.lastIndexOf("/");
      const parent = slash === -1 ? "" : id.slice(0, slash);
      if (parent === dest) return;
      void commitMove(id, dest);
    },
  });

  const commitRename = async (entry: FsEntry, newName: string) => {
    setRenamingPath(null);
    if (newName === entry.name) return;
    try {
      const result = await ipc.notesRename(entry.relPath, newName);
      onRenamed(entry.relPath, result);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const confirmDelete = async (entry: FsEntry) => {
    const what =
      entry.kind === "dir"
        ? `папку «${entry.name}» со всем содержимым`
        : `заметку «${entry.name}»`;
    if (!confirmRemove(what)) return;
    try {
      await ipc.notesDelete(entry.relPath);
      onDeleted(entry.relPath);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const startCreate = (kind: "dir" | "file") => {
    setRenamingPath(null);
    setCreating({ parentRel: selectedDirRel, kind });
  };

  useEffect(() => {
    if (!createTick) return;
    startCreate("file");
    // selectedDirRel is intentionally not a dep: only a bumped tick opens the
    // create input, folder selection changes must not.
  }, [createTick]);

  const commitCreate = async (kind: "dir" | "file", parentRel: string, name: string) => {
    setCreating(null);
    // A folder is created by seeding it with a note (see FOLDER_SEED_NOTE).
    const rel =
      kind === "file"
        ? joinRel(parentRel, noteFileName(name))
        : joinRel(joinRel(parentRel, name), FOLDER_SEED_NOTE);
    try {
      const created = await ipc.notesCreate(rel, "");
      if (kind === "dir") {
        const folderRel = rel.slice(0, rel.lastIndexOf("/"));
        setOpenPaths((prev) => ({ ...prev, [folderRel]: true }));
      }
      onSelect(created);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  const createRow = (
    <li className="tree-node" key="__creating">
      <div className="tree-row">
        <span className={creating?.kind === "dir" ? "icon folder-icon" : "icon"} />
        <RenameInput
          value=""
          placeholder={creating?.kind === "dir" ? "Имя новой папки…" : "Имя новой заметки…"}
          onCommit={(name) => void commitCreate(creating!.kind, creating!.parentRel, name)}
          onCancel={() => setCreating(null)}
        />
      </div>
    </li>
  );

  const renderNode = (entry: FsEntry) => {
    const open = !!openPaths[entry.relPath];
    return (
      <li className="tree-node" key={entry.relPath}>
        <NotesRow
          entry={entry}
          open={open}
          selected={selectedRelPath === entry.relPath}
          renaming={renamingPath === entry.relPath}
          dragging={draggingId === entry.relPath}
          dropOver={!!dropTarget && dropTarget.container === entry.relPath}
          noDrop={
            !!dropTarget && dropTarget.container === entry.relPath && !canDrop(draggingId ?? "", entry.relPath)
          }
          onToggle={toggle}
          onSelect={onSelect}
          onStartRename={(e) => {
            setCreating(null);
            setRenamingPath(e.relPath);
          }}
          onRename={(e, name) => void commitRename(e, name)}
          onRenameCancel={() => setRenamingPath(null)}
          onDelete={(e) => void confirmDelete(e)}
          onPointerDown={(e, ev) => onPointerDown(ev, e.relPath)}
          consumeSuppressedClick={consumeSuppressedClick}
        />
        {entry.kind === "dir" && open && (
          <ul className="tree-children nested">
            {(childrenByDir[entry.relPath] ?? []).map(renderNode)}
            {creating?.parentRel === entry.relPath && createRow}
          </ul>
        )}
      </li>
    );
  };

  const root = childrenByDir[""] ?? [];
  let body;
  if (loading && Object.keys(childrenByDir).length === 0) {
    body = <div className="empty-note">Загрузка…</div>;
  } else if (root.length === 0 && !creating) {
    body = <div className="empty-note">Заметок пока нет</div>;
  } else {
    body = (
      <ul className="tree-children root">
        {root.map(renderNode)}
        {creating?.parentRel === "" && createRow}
      </ul>
    );
  }

  return (
    <>
      <div className="tree-toolbar notes-toolbar">        <button className="ghost-btn" onClick={() => startCreate("file")}>
          <IconFilePlus />
          Новая заметка
        </button>
        <button className="ghost-btn" onClick={() => startCreate("dir")}>
          <IconFolderPlus />
          Новая папка
        </button>
        {noteTemplates.length > 0 && (
          <select
            className="tf-select"
            value=""
            title="Создать заметку из шаблона"
            onChange={(e) => void handleTemplate(e.target.value)}
          >
            <option value="">Из шаблона…</option>
            {noteTemplates.map((t) => (
              <option key={t.name} value={t.name}>
                {t.name}
              </option>
            ))}
          </select>
        )}
      </div>
      {error && <div className="tree-error">{error}</div>}
      <div
        className={dropTarget?.container === "" ? "notes-tree drop-over" : "notes-tree"}
        data-drop-dir=""
      >
        {body}
      </div>
    </>
  );
}
