import { useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import { confirmRemove } from "../../shared/confirm";
import { usePointerDrag } from "../../hooks/usePointerDrag";
import ContextMenu from "../shared/ContextMenu";
import { toMessage, useFileOps } from "./fileOps";
import { fileMenuItems, useFileMenuActions } from "./fileMenu";
import { useFileCrossActions } from "../shared/crossActions";
import { isDocument, useOpenWith } from "./useOpenWith";
import FileRow from "./FileRow";
import RenameInput from "./RenameInput";
import { useOsFileDrag } from "./useOsFileDrag";
import { IconFile, IconFilePlus, IconFolder, IconFolderPlus } from "./icons";
import ViewToggle from "./ViewToggle";

interface Props {
  projectId: number;
  treeVersion: number;
  selectedRelPath: string | null;
  selectedDirRel: string;
  onSelect: (entry: FsEntry) => void;
  view: "list" | "grid";
  onViewChange: (view: "list" | "grid") => void;
}

export default function FileTree({
  projectId,
  treeVersion,
  selectedRelPath,
  selectedDirRel,
  onSelect,
  view,
  onViewChange,
}: Props) {
  const [childrenByDir, setChildrenByDir] = useState<Record<string, FsEntry[]>>({});
  const [openPaths, setOpenPaths] = useState<Record<string, boolean>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [renamingPath, setRenamingPath] = useState<string | null>(null);
  const [creating, setCreating] = useState<{ parentRel: string; kind: "dir" | "file" } | null>(
    null,
  );
  const [menu, setMenu] = useState<{ entry: FsEntry; x: number; y: number } | null>(null);
  const openRef = useRef<Set<string>>(new Set());
  const ops = useFileOps(projectId);
  const menuActions = useFileMenuActions(projectId);
  const ow = useOpenWith(projectId);
  const cross = useFileCrossActions(projectId);
  const osHover = useOsFileDrag(
    () => selectedDirRel,
    (dest, paths) => void ops.importFiles(dest, paths),
  );

  useEffect(() => {
    openRef.current = new Set();
    setOpenPaths({});
    setRenamingPath(null);
    setCreating(null);
  }, [projectId]);

  // Refetch root plus every open folder on project switch and fs-changed.
  // A renamed/removed open folder may fail to list; drop it instead of failing the tree.
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    const dirs = ["", ...openRef.current];
    void (async () => {
      const results = await Promise.allSettled(dirs.map((d) => ipc.fsListDir(projectId, d)));
      if (cancelled) return;
      const next: Record<string, FsEntry[]> = {};
      let rootError: string | null = null;
      results.forEach((res, i) => {
        if (res.status === "fulfilled") next[dirs[i]] = res.value;
        else if (dirs[i] === "") rootError = toMessage(res.reason);
      });
      setChildrenByDir(next);
      setError(rootError);
      setLoading(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [projectId, treeVersion]);

  const toggle = async (entry: FsEntry) => {
    const path = entry.relPath;
    if (openPaths[path]) {
      openRef.current.delete(path);
      setOpenPaths((prev) => {
        const next = { ...prev };
        delete next[path];
        return next;
      });
      return;
    }
    if (!childrenByDir[path]) {
      try {
        const list = await ipc.fsListDir(projectId, path);
        setChildrenByDir((prev) => ({ ...prev, [path]: list }));
      } catch (e) {
        setError(toMessage(e));
        return;
      }
    }
    openRef.current.add(path);
    setOpenPaths((prev) => ({ ...prev, [path]: true }));
  };

  const commitRename = async (entry: FsEntry, newName: string) => {
    setRenamingPath(null);
    if (newName === entry.name) return;
    const result = await ops.rename(entry.relPath, newName);
    if (!result) return;
    const oldPath = entry.relPath;
    if (openRef.current.has(oldPath)) {
      openRef.current.delete(oldPath);
      openRef.current.add(result.relPath);
      setOpenPaths((prev) => {
        const next = { ...prev };
        delete next[oldPath];
        next[result.relPath] = true;
        return next;
      });
      setChildrenByDir((prev) => {
        const kids = prev[oldPath];
        if (!kids) return prev;
        const next = { ...prev };
        delete next[oldPath];
        next[result.relPath] = kids;
        return next;
      });
    }
    if (selectedRelPath === oldPath) onSelect(result);
  };

  const confirmDelete = async (entry: FsEntry) => {
    const what =
      entry.kind === "dir"
        ? `папку «${entry.name}» со всем содержимым`
        : `файл «${entry.name}»`;
    if (!confirmRemove(what)) return;
    await ops.remove(entry.relPath);
  };

  const startCreate = async (kind: "dir" | "file") => {
    setRenamingPath(null);
    const parent = selectedDirRel;
    if (parent && !openPaths[parent]) {
      if (!childrenByDir[parent]) {
        try {
          const list = await ipc.fsListDir(projectId, parent);
          setChildrenByDir((prev) => ({ ...prev, [parent]: list }));
        } catch (e) {
          setError(toMessage(e));
          return;
        }
      }
      openRef.current.add(parent);
      setOpenPaths((prev) => ({ ...prev, [parent]: true }));
    }
    setCreating({ parentRel: parent, kind });
  };

  const commitCreate = async (kind: "dir" | "file", parentRel: string, name: string) => {
    setCreating(null);
    if (kind === "dir") await ops.createFolder(parentRel, name);
    else await ops.createFile(parentRel, name);
  };

  // Backend also rejects these moves; the UI guard only prevents the attempt.
  const canDrop = (src: string | null, dest: string) =>
    !!src && dest !== src && !dest.startsWith(`${src}/`);

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
      void (async () => {
        const moved = await ops.move(id, dest);
        // Keep the preview/selection alive on the file's new path.
        if (moved && selectedRelPath === id) onSelect(moved);
      })();
    },
  });

  const createRow = () => (
    <li className="tree-node" key="__creating">
      <div className="tree-row">
        <span className={creating?.kind === "dir" ? "icon folder-icon" : "icon"}>
          {creating?.kind === "dir" ? <IconFolder /> : <IconFile />}
        </span>
        <RenameInput
          value=""
          placeholder={creating?.kind === "dir" ? "Имя новой папки…" : "Имя нового файла…"}
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
        <FileRow
          entry={entry}
          open={open}
          selected={selectedRelPath === entry.relPath}
          renaming={renamingPath === entry.relPath}
          dragging={draggingId === entry.relPath}
          dropOver={dropTarget?.container === entry.relPath || osHover === entry.relPath}
          noDrop={
            dropTarget?.container === entry.relPath && !canDrop(draggingId, entry.relPath)
          }
          onToggle={toggle}
          onSelect={onSelect}
          onStartRename={(e) => {
            setCreating(null);
            setRenamingPath(e.relPath);
          }}
          onRename={(e, name) => void commitRename(e, name)}
          onRenameCancel={() => setRenamingPath(null)}
          onCopyAs={(e) => menuActions.copyAs(e)}
          onDelete={(e) => void confirmDelete(e)}
          onPointerDown={
            entry.kind === "file" ? (e, ev) => onPointerDown(ev, e.relPath) : undefined
          }
          consumeSuppressedClick={consumeSuppressedClick}
          onContextMenu={(e, ev) => setMenu({ entry: e, x: ev.clientX, y: ev.clientY })}
          onOpenExternal={(e) =>
            void (isDocument(e) ? ow.openDocument(e) : ops.openExternal(e.relPath))
          }
          onOpenWith={isDocument(entry) ? (e) => void ow.showChooser(e) : undefined}
          onAskAgent={entry.kind === "file" ? cross.askAgent : undefined}
        />
        {entry.kind === "dir" && open && (
          <ul className="tree-children nested">
            {(childrenByDir[entry.relPath] ?? []).map(renderNode)}
            {creating?.parentRel === entry.relPath && createRow()}
          </ul>
        )}
      </li>
    );
  };

  const root = childrenByDir[""] ?? [];
  const rootDropOver = dropTarget?.container === "" || osHover === "";
  let body;
  if (loading && !childrenByDir[""]) {
    body = <div className="empty-note">Загрузка…</div>;
  } else if (error) {
    body = <div className="empty-note">{error}</div>;
  } else if (root.length === 0 && !creating) {
    body = (
      <div
        className={osHover === "" ? "empty-note drop-over" : "empty-note"}
        data-drop-dir=""
      >
        Проект пуст
      </div>
    );
  } else {
    body = (
      <div className={rootDropOver ? "file-tree drop-over" : "file-tree"} data-drop-dir="">
        <ul className="tree-children root">
          {root.map(renderNode)}
          {creating?.parentRel === "" && createRow()}
        </ul>
      </div>
    );
  }

  return (
    <>
      <div className="tree-toolbar">
        <button className="ghost-btn" onClick={() => void startCreate("dir")}>
          <IconFolderPlus />
          Новая папка
        </button>
        <button className="ghost-btn" onClick={() => void startCreate("file")}>
          <IconFilePlus />
          Новый файл
        </button>
        <ViewToggle view={view} onChange={onViewChange} />
      </div>
      {ops.error && <div className="tree-error">{ops.error}</div>}
      {ow.error && <div className="tree-error">{ow.error}</div>}
      {body}
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
