import { useState } from "react";
import * as ipc from "../../api/ipc";

import { toMessage } from "../../shared/errors";

export { toMessage } from "../../shared/errors";

// Mutation calls shared by tree and grid. The backend emits fs-changed after
// each mutation, so views refresh via treeVersion; no manual refetch here.
export function useFileOps(projectId: number) {
  const [error, setError] = useState<string | null>(null);

  async function run<T>(fn: () => Promise<T>): Promise<T | null> {
    try {
      const result = await fn();
      setError(null);
      return result;
    } catch (e) {
      setError(toMessage(e));
      return null;
    }
  }

  return {
    error,
    createFolder: (parentRel: string, name: string) =>
      run(() => ipc.fsCreateFolder(projectId, parentRel, name)),
    createFile: (parentRel: string, name: string) =>
      run(() => ipc.fsCreateFile(projectId, parentRel, name)),
    rename: (relPath: string, newName: string) =>
      run(() => ipc.fsRename(projectId, relPath, newName)),
    remove: (relPath: string) => run(() => ipc.fsDelete(projectId, relPath)),
    move: (fromRel: string, toDirRel: string) =>
      run(() => ipc.fsMove(projectId, fromRel, toDirRel)),
    importFiles: (destRel: string, sourcePaths: string[]) =>
      run(() => ipc.fsImport(projectId, destRel, sourcePaths)),
    openExternal: (relPath: string) => run(() => ipc.fsOpenExternal(projectId, relPath)),
  };
}
