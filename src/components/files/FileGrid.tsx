import { useEffect, useState, type DragEvent } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { toMessage, useFileOps } from "./fileOps";
import { fileMenuItems, useFileMenuActions } from "./fileMenu";
import ContextMenu from "../shared/ContextMenu";
import { useFileCrossActions } from "../shared/crossActions";
import { isDocument, useOpenWith } from "./useOpenWith";
import RenameInput from "./RenameInput";
import { useOsFileDrag } from "./useOsFileDrag";
import {
  EntryIcon,
  IconAskAgent,
  IconCopy,
  IconFile,
  IconFilePlus,
  IconFolder,
  IconFolderPlus,
  IconOpenWith,
  IconPencil,
  IconTrash,
} from "./icons";
import ViewToggle from "./ViewToggle";

interface Props {
  projectId: number;
  treeVersion: number;
  currentDir: string;
  selectedRelPath: string | null;
  onSelect: (entry: FsEntry) => void;
  onNavigateDir: (dirRel: string) => void;
  view: "list" | "grid";
  onViewChange: (view: "list" | "grid") => void;
}

export default function FileGrid({
  projectId,
  treeVersion,
  currentDir,
  selectedRelPath,
  onSelect,
  onNavigateDir,
  view,
  onViewChange,
}: Props) {
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [renamingPath, setRenamingPath] = useState<string | null>(null);
  const [creating, setCreating] = useState<{ parentRel: string; kind: "dir" | "file" } | null>(
    null,
  );
  const [dragRel, setDragRel] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ entry: FsEntry; x: number; y: number } | null>(null);
  const ops = useFileOps(projectId);
  const menuActions = useFileMenuActions(projectId);
  const ow = useOpenWith(projectId);
  const cross = useFileCrossActions(projectId);
  const osHover = useOsFileDrag(
    () => currentDir,
    (dest, paths) => void ops.importFiles(dest, paths),
  );

  useEffect(() => {
    setRenamingPath(null);
    setCreating(null);
  }, [projectId, currentDir]);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    void ipc
      .fsListDir(projectId, currentDir)
      .then((list) => {
        if (!cancelled) {
          setEntries(list);
          setError(null);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(toMessage(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [projectId, treeVersion, currentDir]);

  const commitRename = async (entry: FsEntry, newName: string) => {
    setRenamingPath(null);
    if (newName === entry.name) return;
    const result = await ops.rename(entry.relPath, newName);
    if (result && selectedRelPath === entry.relPath) onSelect(result);
  };

  const confirmDelete = async (entry: FsEntry) => {
    const what =
      entry.kind === "dir"
        ? `папку «${entry.name}» со всем содержимым`
        : `файл «${entry.name}»`;
    if (!window.confirm(`Удалить ${what}?`)) return;
    await ops.remove(entry.relPath);
  };

  const commitCreate = async (kind: "dir" | "file", parentRel: string, name: string) => {
    setCreating(null);
    if (kind === "dir") await ops.createFolder(parentRel, name);
    else await ops.createFile(parentRel, name);
  };

  // Double-click: a folder opens (navigate into it), a file opens its preview
  // (same as single-click selection). Suppressed while a tile is being renamed;
  // the action buttons stop the double-click from bubbling here.
  const handleDoubleClick = (entry: FsEntry) => {
    if (renamingPath === entry.relPath) return;
    if (entry.kind === "dir") onNavigateDir(entry.relPath);
    else onSelect(entry);
  };

  // Backend also rejects these moves; the UI guard only prevents the attempt.
  const canDrop = (src: string | null, dest: string) =>
    !!src && dest !== src && !dest.startsWith(`${src}/`);

  const endDrag = () => {
    setDragRel(null);
    setDropTarget(null);
  };

  const handleDragStart = (entry: FsEntry, e: DragEvent<HTMLDivElement>) => {
    e.dataTransfer.setData("text/plain", entry.relPath);
    e.dataTransfer.effectAllowed = "move";
    setDragRel(entry.relPath);
  };

  const handleDirDragOver = (dirRel: string, e: DragEvent<HTMLDivElement>) => {
    if (canDrop(dragRel, dirRel)) e.preventDefault();
    setDropTarget(dirRel);
  };

  const handleDirDrop = (dirRel: string, e: DragEvent<HTMLDivElement>) => {
    const rel = e.dataTransfer.getData("text/plain");
    endDrag();
    if (rel && canDrop(rel, dirRel)) void ops.move(rel, dirRel);
  };

  const handleRootDragOver = (e: DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    setDropTarget(currentDir);
  };

  const handleRootDrop = (e: DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    const rel = e.dataTransfer.getData("text/plain");
    endDrag();
    if (rel) void ops.move(rel, currentDir);
  };

  if (loading && entries.length === 0) return <div className="empty-note">Загрузка…</div>;

  return (
    <>
      <div className="tree-toolbar">
        <button
          className="ghost-btn"
          onClick={() => setCreating({ parentRel: currentDir, kind: "dir" })}
        >
          <IconFolderPlus />
          Новая папка
        </button>
        <button
          className="ghost-btn"
          onClick={() => setCreating({ parentRel: currentDir, kind: "file" })}
        >
          <IconFilePlus />
          Новый файл
        </button>
        <ViewToggle view={view} onChange={onViewChange} />
      </div>
      {ops.error && <div className="tree-error">{ops.error}</div>}
      {ow.error && <div className="tree-error">{ow.error}</div>}
      {error ? (
        <div className="empty-note">{error}</div>
      ) : entries.length === 0 && !creating ? (
        <div
          className={osHover === currentDir ? "empty-note drop-over" : "empty-note"}
          data-drop-dir={currentDir}
        >
          Проект пуст
        </div>
      ) : (
        <div
          className={
            dropTarget === currentDir || osHover === currentDir
              ? "file-grid drop-over"
              : "file-grid"
          }
          data-drop-dir={currentDir}
          onDragOver={handleRootDragOver}
          onDrop={handleRootDrop}
          onDragLeave={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDropTarget(null);
          }}
        >
          {entries.map((entry) => (
            <div
              className={[
                "tile",
                selectedRelPath === entry.relPath ? "selected" : "",
                dropTarget === entry.relPath || osHover === entry.relPath ? "drop-over" : "",
                dropTarget === entry.relPath && !canDrop(dragRel, entry.relPath) ? "no-drop" : "",
              ]
                .filter(Boolean)
                .join(" ")}
              key={entry.relPath}
              data-drop-dir={entry.kind === "dir" ? entry.relPath : undefined}
              draggable
              onDragStart={(e) => handleDragStart(entry, e)}
              onDragEnd={endDrag}
              onDragOver={
                entry.kind === "dir"
                  ? (e) => {
                      e.stopPropagation();
                      handleDirDragOver(entry.relPath, e);
                    }
                  : undefined
              }
              onDrop={
                entry.kind === "dir"
                  ? (e) => {
                      e.stopPropagation();
                      e.preventDefault();
                      handleDirDrop(entry.relPath, e);
                    }
                  : undefined
              }
              onClick={() => onSelect(entry)}
              onDoubleClick={() => handleDoubleClick(entry)}
              onContextMenu={(e) => {
                if (renamingPath === entry.relPath) return;
                // Right-click must not select/open or start a drag: the menu owns it.
                e.preventDefault();
                e.stopPropagation();
                setMenu({ entry, x: e.clientX, y: e.clientY });
              }}
            >
              <span className="tile-actions" onDoubleClick={(e) => e.stopPropagation()}>
                {entry.kind === "file" && (
                  <button
                    className="mini-btn"
                    title="Спросить агента"
                    onClick={(e) => {
                      e.stopPropagation();
                      cross.askAgent(entry);
                    }}
                  >
                    <IconAskAgent />
                  </button>
                )}
                {isDocument(entry) && (
                  <button
                    className="mini-btn"
                    title="Открыть с помощью…"
                    onClick={(e) => {
                      e.stopPropagation();
                      void ow.showChooser(entry);
                    }}
                  >
                    <IconOpenWith />
                  </button>
                )}
                <button
                  className="mini-btn"
                  title="Переименовать"
                  onClick={(e) => {
                    e.stopPropagation();
                    setRenamingPath(entry.relPath);
                  }}
                >
                  <IconPencil />
                </button>
                <button
                  className="mini-btn"
                  title="Скопировать"
                  onClick={(e) => {
                    e.stopPropagation();
                    menuActions.copyAs(entry);
                  }}
                >
                  <IconCopy />
                </button>
                <button
                  className="mini-btn danger"
                  title="Удалить"
                  onClick={(e) => {
                    e.stopPropagation();
                    void confirmDelete(entry);
                  }}
                >
                  <IconTrash />
                </button>
              </span>
              <span className={entry.kind === "dir" ? "tile-icon folder-icon" : "tile-icon"}>
                <EntryIcon entry={entry} />
              </span>
              {renamingPath === entry.relPath ? (
                <RenameInput
                  value={entry.name}
                  selectBase={entry.kind === "file"}
                  onCommit={(name) => void commitRename(entry, name)}
                  onCancel={() => setRenamingPath(null)}
                />
              ) : (
                <span className="label">{entry.name}</span>
              )}
              {entry.ext !== "" && <span className="tag">{entry.ext}</span>}
            </div>
          ))}
          {creating && (
            <div className="tile">
              <span className={creating.kind === "dir" ? "tile-icon folder-icon" : "tile-icon"}>
                {creating.kind === "dir" ? <IconFolder /> : <IconFile />}
              </span>
              <RenameInput
                value=""
                placeholder={creating.kind === "dir" ? "Имя новой папки…" : "Имя нового файла…"}
                onCommit={(name) => void commitCreate(creating.kind, creating.parentRel, name)}
                onCancel={() => setCreating(null)}
              />
            </div>
          )}
        </div>
      )}
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={fileMenuItems(menu.entry, menuActions, () => {
            setCreating(null);
            setRenamingPath(menu.entry.relPath);
          })}
          onClose={() => setMenu(null)}
        />
      )}
      {ow.dialog}
    </>
  );
}
