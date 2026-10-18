import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import * as ipc from "../../api/ipc";
import type { ConfigKind } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";

export interface DebouncedConfig<T> {
  form: T | null;
  updateForm: (patch: Partial<T>) => void;
  reload: () => void;
  flush: () => Promise<void>;
  focusField: (name: keyof T) => () => void;
  blurField: () => void;
  // Name of the field the user is editing (null when nothing is focused), so
  // tab-level effects can guard on it the way the inline code did.
  focusedRef: RefObject<keyof T | null>;
}

// Read-modify-write of one config file, shared by the settings tabs. Edits are
// debounced (500 ms), blur flushes immediately, the patch starts from the last
// raw read so unknown keys survive, and a hot reload never clobbers the field
// the user is editing. The config is read on mount; a pending edit is flushed
// on unmount. `toPatch(form, raw)` returns the full object to write.
export function useDebouncedConfig<T>(
  kind: ConfigKind,
  toForm: (raw: Record<string, unknown>) => T,
  toPatch: (form: T, raw: Record<string, unknown>) => Record<string, unknown>,
  onError: (message: string) => void,
  onLoaded?: (form: T) => void,
): DebouncedConfig<T> {
  const [form, setForm] = useState<T | null>(null);
  // Last raw config object as read from disk, so unknown keys survive writes.
  const rawRef = useRef<Record<string, unknown> | null>(null);
  const formRef = useRef<T | null>(null);
  // Name of the field the user is editing; hot-reload never overwrites it.
  const focusedRef = useRef<keyof T | null>(null);
  const saveTimer = useRef<number | undefined>(undefined);

  // Callbacks live in refs so the returned functions keep stable identities.
  const toFormRef = useRef(toForm);
  const toPatchRef = useRef(toPatch);
  const onErrorRef = useRef(onError);
  const onLoadedRef = useRef(onLoaded);
  toFormRef.current = toForm;
  toPatchRef.current = toPatch;
  onErrorRef.current = onError;
  onLoadedRef.current = onLoaded;

  const flush = useCallback((): Promise<void> => {
    window.clearTimeout(saveTimer.current);
    const raw = rawRef.current;
    const f = formRef.current;
    if (!raw || !f) return Promise.resolve();
    return ipc
      .writeConfig(kind, toPatchRef.current(f, raw))
      .catch((e) => onErrorRef.current(errMsg(e)));
  }, [kind]);

  const scheduleSave = useCallback(() => {
    window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(flush, 500);
  }, [flush]);

  const updateForm = useCallback(
    (patch: Partial<T>) => {
      if (!formRef.current) return;
      const next = { ...formRef.current, ...patch };
      formRef.current = next;
      setForm(next);
      scheduleSave();
    },
    [scheduleSave],
  );

  // Re-read the file and refresh the form, keeping the focused field as-is so
  // hand-edits to the file show up without clobbering in-progress typing.
  const reload = useCallback(() => {
    void ipc
      .readConfig(kind)
      .then((v) => {
        rawRef.current = v as Record<string, unknown>;
        const next = toFormRef.current(rawRef.current);
        const focused = focusedRef.current;
        if (focused && formRef.current) {
          const nextRow = next as unknown as Record<string | number | symbol, unknown>;
          nextRow[focused] = formRef.current[focused];
        }
        formRef.current = next;
        setForm(next);
        onLoadedRef.current?.(next);
      })
      .catch((e) => onErrorRef.current(errMsg(e)));
  }, [kind]);

  useEffect(() => {
    reload();
    return () => {
      window.clearTimeout(saveTimer.current);
      void flush();
    };
  }, [reload, flush]);

  const focusField = useCallback(
    (name: keyof T) => () => {
      focusedRef.current = name;
    },
    [],
  );
  const blurField = useCallback(() => {
    focusedRef.current = null;
    void flush();
  }, [flush]);

  return { form, updateForm, reload, flush, focusField, blurField, focusedRef };
}
