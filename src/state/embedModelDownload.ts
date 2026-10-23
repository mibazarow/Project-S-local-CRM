// One-shot embed model download store: after onboarding the app pulls the
// EmbeddingGemma 2 model (~300 MB) from Hugging Face once. The store is a
// module-level useSyncExternalStore so the banner (EmbedModelBootstrap) and
// the settings button (AgentTab) share one in-flight download; FTS-only
// search keeps working when the download fails or the machine is offline.
import { useSyncExternalStore } from "react";
import * as ipc from "../api/ipc";
import { toast } from "../components/shared/toast";
import { toMessage } from "../shared/errors";

export type EmbedModelPhase = "idle" | "checking" | "downloading" | "done" | "error";

export interface EmbedModelState {
  phase: EmbedModelPhase;
  file: string;
  done: number;
  total: number;
  error: string | null;
}

const INITIAL: EmbedModelState = { phase: "idle", file: "", done: 0, total: 0, error: null };

let state: EmbedModelState = INITIAL;
// Non-null while a check/download is in flight: concurrent starts join it
// instead of racing a second download_embed_model command. Cleared when the
// run settles (done or error) so a retry can start a fresh attempt.
let inFlight: Promise<void> | null = null;

const listeners = new Set<() => void>();

function emit(): void {
  for (const l of listeners) l();
}

function set(patch: Partial<EmbedModelState>): void {
  state = { ...state, ...patch };
  emit();
}

export function getSnapshot(): EmbedModelState {
  return state;
}

export function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

export function useEmbedModelDownload(): EmbedModelState {
  return useSyncExternalStore(subscribe, getSnapshot);
}

// Hide the error banner without starting another attempt.
export function dismissEmbedModelError(): void {
  if (state.phase === "error") set({ ...INITIAL });
}

export function startEmbedModelDownload(): Promise<void> {
  if (inFlight) return inFlight;
  inFlight = run().finally(() => {
    inFlight = null;
  });
  return inFlight;
}

async function run(): Promise<void> {
  set({ ...INITIAL, phase: "checking" });
  let unlisten: (() => void) | undefined;
  try {
    const status = await ipc.embedStatus();
    if (status.modelPresent) {
      // Already on disk (bundled install, earlier download): nothing to do,
      // no toast — the app should stay quiet on every later startup.
      set({ ...INITIAL });
      return;
    }
    set({ phase: "downloading" });
    unlisten = await ipc.onEmbedModelProgress((p) => {
      set({ phase: "downloading", file: p.file, done: p.done, total: p.total });
    });
    const after = await ipc.downloadEmbedModel();
    if (!after.modelPresent) {
      throw new Error("model files still missing after download");
    }
    set({ phase: "done" });
    toast("ok", "Модель скачана — поиск по смыслу готов");
    // Best-effort: build the vectors right away so semantic search works
    // without waiting for the scheduled reindex pass.
    try {
      await ipc.reindexStart();
    } catch {
      // The periodic pass will pick the model up.
    }
  } catch (e) {
    set({ phase: "error", error: toMessage(e) });
  } finally {
    unlisten?.();
  }
}
