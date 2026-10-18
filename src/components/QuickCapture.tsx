import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import * as ipc from "../api/ipc";
import { useAppState } from "../state/AppStore";
import Segmented from "./shared/Segmented";

interface Props {
  initialTarget: "note" | "task";
  onClose: () => void;
}

import { toMessage } from "../shared/errors";

export { toMessage } from "../shared/errors";

export default function QuickCapture({ initialTarget, onClose }: Props) {
  const state = useAppState();
  const [target, setTarget] = useState<"note" | "task">(initialTarget);
  const [text, setText] = useState("");
  const [projectId, setProjectId] = useState<number | null>(
    state.activeProjectId ?? state.projects[0]?.id ?? null,
  );
  // nonce re-arms the auto-hide timer when the same message is saved twice
  const [saved, setSaved] = useState<{ msg: string; n: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const textRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    textRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!saved) return;
    const timer = setTimeout(() => setSaved(null), 1800);
    return () => clearTimeout(timer);
  }, [saved]);

  const save = async () => {
    const trimmed = text.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    setError(null);
    try {
      const res = await ipc.quickCapture(trimmed, target, target === "task" ? projectId : null);
      setText("");
      setSaved((s) => ({
        n: (s?.n ?? 0) + 1,
        msg: res.kind === "note" ? "Добавлено в заметки «Входящие»" : "Задача создана",
      }));
    } catch (e) {
      setError(toMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      onClose();
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      void save();
    }
  };

  return (
    <div
      className="capture-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="capture" onKeyDown={onKeyDown}>
        <div className="capture-head">
          <Segmented
            value={target}
            options={[
              { value: "note", label: "Заметка" },
              { value: "task", label: "Задача" },
            ]}
            onChange={setTarget}
          />
          {target === "task" && (
            <select
              className="capture-project"
              value={projectId ?? ""}
              onChange={(e) => setProjectId(e.target.value === "" ? null : Number(e.target.value))}
            >
              {state.projects.length === 0 && <option value="">Нет проектов</option>}
              {state.projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          )}
        </div>
        <textarea
          ref={textRef}
          className="capture-text"
          placeholder={target === "note" ? "Быстрая заметка…" : "Новая задача…"}
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
        {error && <div className="capture-error">{error}</div>}
        {saved && <div className="capture-ok">{saved.msg}</div>}
        <div className="capture-foot">
          <span>⌘Enter — сохранить</span>
          <span>Esc — закрыть</span>
          <button
            className="primary-btn small"
            onClick={() => void save()}
            disabled={!text.trim() || busy}
          >
            Сохранить
          </button>
        </div>
      </div>
    </div>
  );
}
