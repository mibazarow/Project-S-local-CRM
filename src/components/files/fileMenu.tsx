import * as ipc from "../../api/ipc";
import type { FsEntry } from "../../api/types";
import type { MenuItem } from "../shared/ContextMenu";
import { toast } from "../shared/toast";
import { toMessage } from "./fileOps";
import { IconArchive, IconCopy, IconEye, IconLink, IconPencil } from "./icons";

// Archive extensions fs_extract_archive can unpack (backend ARCHIVE_SUFFIXES).
const ARCHIVE_EXTS = ["zip", "tar", "gz", "tgz", "bz2", "xz"];

export function isArchive(entry: FsEntry): boolean {
  return entry.kind === "file" && ARCHIVE_EXTS.includes(entry.ext);
}

// Context-menu actions shared by the list and grid views: results and errors
// surface as toasts; the views refresh themselves via the fs-changed event.
export function useFileMenuActions(projectId: number) {
  async function run<T>(ok: (result: T) => string, fn: () => Promise<T>) {
    try {
      toast("ok", ok(await fn()));
    } catch (e) {
      toast("err", toMessage(e));
    }
  }
  return {
    reveal: (entry: FsEntry) =>
      void run(
        () => "Показано в Finder",
        () => ipc.fsRevealInFinder(projectId, entry.relPath),
      ),
    copyPath: (entry: FsEntry) =>
      void run(
        () => "Путь скопирован в буфер обмена",
        async () =>
          navigator.clipboard.writeText(await ipc.fsAbsolutePath(projectId, entry.relPath)),
      ),
    copyAs: (entry: FsEntry) =>
      void run(
        (item) => `Скопировано в «${item.name}»`,
        () => ipc.fsCopyAs(projectId, entry.relPath),
      ),
    extract: (entry: FsEntry) =>
      void run(
        (item) => `Распаковано в «${item.name}»`,
        () => ipc.fsExtractArchive(projectId, entry.relPath),
      ),
  };
}

export type FileMenuActions = ReturnType<typeof useFileMenuActions>;

// The one file/folder menu, identical in both views; «Распаковать здесь» only
// for archive files, «Переименовать» triggers the view's inline rename.
export function fileMenuItems(
  entry: FsEntry,
  actions: FileMenuActions,
  startRename: () => void,
): MenuItem[] {
  const items: MenuItem[] = [
    { label: "Переименовать", icon: <IconPencil />, onSelect: startRename },
    { label: "Показать в Finder", icon: <IconEye />, onSelect: () => actions.reveal(entry) },
    { label: "Скопировать путь", icon: <IconLink />, onSelect: () => actions.copyPath(entry) },
    { label: "Скопировать", icon: <IconCopy />, onSelect: () => actions.copyAs(entry) },
  ];
  if (isArchive(entry)) {
    items.push({
      label: "Распаковать здесь",
      icon: <IconArchive />,
      onSelect: () => actions.extract(entry),
    });
  }
  return items;
}
