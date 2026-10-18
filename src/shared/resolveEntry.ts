import type { FsEntry } from "../api/types";

// Resolve a search/palette relPath to the real FsEntry through the folder's
// own listing, so previews and editors get correct flags and size.
export async function resolveEntry(
  relPath: string,
  listDir: (dir: string) => Promise<FsEntry[]>,
): Promise<FsEntry | null> {
  const slash = relPath.lastIndexOf("/");
  const dir = slash === -1 ? "" : relPath.slice(0, slash);
  const entries = await listDir(dir);
  return entries.find((e) => e.relPath === relPath) ?? null;
}
