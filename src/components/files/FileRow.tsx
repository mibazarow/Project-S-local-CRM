import type { MouseEvent, PointerEvent as ReactPointerEvent } from "react";
import type { FsEntry } from "../../api/types";
import RenameInput from "./RenameInput";
import {
  EntryIcon,
  IconAskAgent,
  IconChevron,
  IconCopy,
  IconOpenWith,
  IconPencil,
  IconTrash,
} from "./icons";

interface Props {
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
  onRename: (entry: FsEntry, newName: string) => void;
  onRenameCancel: () => void;
  onCopyAs: (entry: FsEntry) => void;
  onDelete: (entry: FsEntry) => void;
  // Pointer drag on every row: files move between folders, folders move
  // into folders/root (self/descendant drops are rejected by the tree).
  onPointerDown?: (entry: FsEntry, e: ReactPointerEvent<HTMLDivElement>) => void;
  consumeSuppressedClick: () => boolean;
  onContextMenu: (entry: FsEntry, e: MouseEvent<HTMLDivElement>) => void;
  onOpenExternal?: (entry: FsEntry) => void;
  onOpenWith?: (entry: FsEntry) => void;
  onAskAgent?: (entry: FsEntry) => void;
}

export default function FileRow({
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
  onCopyAs,
  onDelete,
  onPointerDown,
  consumeSuppressedClick,
  onContextMenu,
  onOpenExternal,
  onOpenWith,
  onAskAgent,
}: Props) {
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
      onPointerDown={
        onPointerDown && !renaming ? (e) => onPointerDown(entry, e) : undefined
      }
      onClick={() => {
        // A drag must not open/toggle the row it ended on.
        if (consumeSuppressedClick()) return;
        if (isDir) onToggle(entry);
        onSelect(entry);
      }}
      onContextMenu={(e) => {
        if (renaming) return;
        // Right-click must not select/open or start a drag: the menu owns it.
        e.preventDefault();
        e.stopPropagation();
        onContextMenu(entry, e);
      }}
      onDoubleClick={() => {
        if (isDir) return;
        // PDFs preview inline now: double-click opens the preview like single click.
        if (entry.ext === "pdf") onSelect(entry);
        else onOpenExternal?.(entry);
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
        {onAskAgent && (
          <button
            className="mini-btn"
            title="Спросить агента"
            onClick={(e) => {
              e.stopPropagation();
              onAskAgent(entry);
            }}
          >
            <IconAskAgent />
          </button>
        )}
        {onOpenWith && (
          <button
            className="mini-btn"
            title="Открыть с помощью…"
            onClick={(e) => {
              e.stopPropagation();
              onOpenWith(entry);
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
            onStartRename(entry);
          }}
        >
          <IconPencil />
        </button>
        <button
          className="mini-btn"
          title="Скопировать"
          onClick={(e) => {
            e.stopPropagation();
            onCopyAs(entry);
          }}
        >
          <IconCopy />
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
