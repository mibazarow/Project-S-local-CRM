import type { DragEvent, MouseEvent } from "react";
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
  dropOver: boolean;
  noDrop: boolean;
  onToggle: (entry: FsEntry) => void;
  onSelect: (entry: FsEntry) => void;
  onStartRename: (entry: FsEntry) => void;
  onRename: (entry: FsEntry, newName: string) => void;
  onRenameCancel: () => void;
  onCopyAs: (entry: FsEntry) => void;
  onDelete: (entry: FsEntry) => void;
  onDragStart: (entry: FsEntry) => void;
  onDragEnd: () => void;
  onDirDragOver: (dirRel: string, e: DragEvent<HTMLDivElement>) => void;
  onDirDrop: (dirRel: string, e: DragEvent<HTMLDivElement>) => void;
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
  dropOver,
  noDrop,
  onToggle,
  onSelect,
  onStartRename,
  onRename,
  onRenameCancel,
  onCopyAs,
  onDelete,
  onDragStart,
  onDragEnd,
  onDirDragOver,
  onDirDrop,
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
    dropOver ? "drop-over" : "",
    noDrop ? "no-drop" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <div
      className={classes}
      data-drop-dir={isDir ? entry.relPath : undefined}
      draggable={!renaming}
      onDragStart={(e) => {
        e.stopPropagation();
        e.dataTransfer.setData("text/plain", entry.relPath);
        e.dataTransfer.effectAllowed = "move";
        onDragStart(entry);
      }}
      onDragEnd={onDragEnd}
      onDragOver={
        isDir
          ? (e) => {
              e.stopPropagation();
              onDirDragOver(entry.relPath, e);
            }
          : undefined
      }
      onDrop={
        isDir
          ? (e) => {
              e.stopPropagation();
              e.preventDefault();
              onDirDrop(entry.relPath, e);
            }
          : undefined
      }
      onClick={() => {
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
