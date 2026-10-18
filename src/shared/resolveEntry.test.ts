import { describe, expect, it, vi } from "vitest";
import type { FsEntry } from "../api/types";
import { resolveEntry } from "./resolveEntry";

const entry = (relPath: string): FsEntry => ({
  name: relPath.split("/").pop() ?? relPath,
  relPath,
  kind: "file",
  ext: "md",
  size: 1,
  modified: 0,
  isMarkdown: true,
  isText: true,
  isImage: false,
});

describe("resolveEntry", () => {
  it("lists the parent directory for a nested path", async () => {
    const listDir = vi.fn(async () => [] as FsEntry[]);
    await resolveEntry("a/b/c.md", listDir);
    expect(listDir).toHaveBeenCalledWith("a/b");
  });

  it("lists the root for a path without a slash", async () => {
    const listDir = vi.fn(async () => [] as FsEntry[]);
    await resolveEntry("c.md", listDir);
    expect(listDir).toHaveBeenCalledWith("");
  });

  it("returns the entry whose relPath matches exactly", async () => {
    const listDir = vi.fn(async () => [entry("a/b.md"), entry("a/bc.md")]);
    const found = await resolveEntry("a/b.md", listDir);
    expect(found?.relPath).toBe("a/b.md");
  });

  it("returns null when the entry is gone", async () => {
    const listDir = vi.fn(async () => [entry("a/other.md")]);
    expect(await resolveEntry("a/gone.md", listDir)).toBeNull();
  });

  it("propagates a listing failure to the caller", async () => {
    const listDir = vi.fn(async () => {
      throw { message: "Нет доступа" };
    });
    await expect(resolveEntry("a/b.md", listDir)).rejects.toEqual({ message: "Нет доступа" });
  });
});
