import { beforeEach, describe, expect, it, vi } from "vitest";
import type { EmbedModelProgress, EmbedStatus } from "../api/types";

const embedStatus = vi.fn();
const downloadEmbedModel = vi.fn();
const reindexStart = vi.fn();
const onEmbedModelProgress = vi.fn();
const toastMock = vi.fn();

vi.mock("../api/ipc", () => ({
  embedStatus: () => embedStatus(),
  downloadEmbedModel: () => downloadEmbedModel(),
  reindexStart: () => reindexStart(),
  onEmbedModelProgress: (h: (p: EmbedModelProgress) => void) => onEmbedModelProgress(h),
}));

vi.mock("../components/shared/toast", () => ({
  toast: (...args: unknown[]) => toastMock(...args),
}));

const status = (modelPresent: boolean): EmbedStatus => ({
  backend: modelPresent ? "ort" : "noop",
  dim: 768,
  available: modelPresent,
  modelPresent,
  reason: modelPresent ? null : "model files missing",
});

let progressHandler: ((p: EmbedModelProgress) => void) | null = null;

beforeEach(() => {
  vi.resetModules();
  progressHandler = null;
  toastMock.mockReset();
  embedStatus.mockReset().mockResolvedValue(status(false));
  downloadEmbedModel.mockReset().mockResolvedValue(status(true));
  reindexStart
    .mockReset()
    .mockResolvedValue({ running: true, done: 0, total: 0, phase: "", nextRunAt: 0, lastRunAt: null });
  onEmbedModelProgress.mockReset().mockImplementation((h: (p: EmbedModelProgress) => void) => {
    progressHandler = h;
    return Promise.resolve(() => {
      progressHandler = null;
    });
  });
});

// Fresh module instance per test: the store is a module-level singleton.
const load = () => import("./embedModelDownload");

describe("embedModelDownload store", () => {
  it("skips the download when the model is already present", async () => {
    embedStatus.mockResolvedValue(status(true));
    const m = await load();
    await m.startEmbedModelDownload();
    expect(embedStatus).toHaveBeenCalledTimes(1);
    expect(downloadEmbedModel).not.toHaveBeenCalled();
    expect(toastMock).not.toHaveBeenCalled();
    expect(m.getSnapshot().phase).toBe("idle");
  });

  it("downloads with progress, toasts once on completion and arms reindex", async () => {
    let resolveDownload: (s: EmbedStatus) => void = () => {};
    downloadEmbedModel.mockImplementation(
      () => new Promise<EmbedStatus>((res) => (resolveDownload = res)),
    );
    const m = await load();
    const p = m.startEmbedModelDownload();
    expect(m.getSnapshot().phase).toBe("checking");
    await vi.waitFor(() => expect(onEmbedModelProgress).toHaveBeenCalledTimes(1));
    expect(m.getSnapshot().phase).toBe("downloading");
    progressHandler!({ file: "model_quantized.onnx_data", done: 50, total: 100 });
    const mid = m.getSnapshot();
    expect(mid.phase).toBe("downloading");
    expect(mid.file).toBe("model_quantized.onnx_data");
    expect(mid.done).toBe(50);
    expect(mid.total).toBe(100);
    resolveDownload(status(true));
    await p;
    expect(m.getSnapshot().phase).toBe("done");
    expect(toastMock).toHaveBeenCalledTimes(1);
    expect(toastMock).toHaveBeenCalledWith("ok", "Модель скачана — поиск по смыслу готов");
    expect(reindexStart).toHaveBeenCalledTimes(1);
  });

  it("transitions to error and a retry starts a fresh attempt", async () => {
    downloadEmbedModel.mockRejectedValue({ message: "offline" });
    const m = await load();
    await m.startEmbedModelDownload();
    expect(m.getSnapshot().phase).toBe("error");
    expect(m.getSnapshot().error).toBe("offline");
    expect(toastMock).not.toHaveBeenCalled();
    await m.startEmbedModelDownload();
    expect(embedStatus).toHaveBeenCalledTimes(2);
    expect(downloadEmbedModel).toHaveBeenCalledTimes(2);
  });

  it("dismiss hides the error without starting another attempt", async () => {
    downloadEmbedModel.mockRejectedValue({ message: "offline" });
    const m = await load();
    await m.startEmbedModelDownload();
    expect(m.getSnapshot().phase).toBe("error");
    m.dismissEmbedModelError();
    expect(m.getSnapshot().phase).toBe("idle");
    expect(m.getSnapshot().error).toBeNull();
    expect(embedStatus).toHaveBeenCalledTimes(1);
  });

  it("concurrent starts join one in-flight run", async () => {
    const m = await load();
    const a = m.startEmbedModelDownload();
    const b = m.startEmbedModelDownload();
    expect(a).toBe(b);
    await a;
    expect(embedStatus).toHaveBeenCalledTimes(1);
    expect(downloadEmbedModel).toHaveBeenCalledTimes(1);
  });

  it("a failed reindex kick still leaves the done state", async () => {
    reindexStart.mockRejectedValue(new Error("reindex busy"));
    const m = await load();
    await m.startEmbedModelDownload();
    expect(m.getSnapshot().phase).toBe("done");
    expect(toastMock).toHaveBeenCalledTimes(1);
  });
});
