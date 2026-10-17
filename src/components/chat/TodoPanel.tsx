import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { TodoDto } from "../../api/types";

interface Props {
  scopeKind: "project" | "session";
  scopeId: number | null;
}

// Read-only plan panel above the chat messages: the agent maintains the list
// via todowrite/todoread; the panel loads it when the scope opens and reloads
// on the scope's "todos-changed" event. Hidden while the plan is empty.
export default function TodoPanel({ scopeKind, scopeId }: Props) {
  const [todos, setTodos] = useState<TodoDto[]>([]);

  useEffect(() => {
    setTodos([]);
    if (scopeId == null) return;
    let cancelled = false;
    const load = () =>
      void ipc
        .todosList(scopeKind, scopeId)
        .then((list) => !cancelled && setTodos(list))
        .catch(() => {});
    load();
    let unlisten: (() => void) | undefined;
    void ipc
      .onTodosChanged((p) => {
        if (p.scopeKind === scopeKind && p.scopeId === scopeId) load();
      })
      .then((u) => {
        if (cancelled) u();
        else unlisten = u;
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [scopeKind, scopeId]);

  if (todos.length === 0) return null;
  const done = todos.filter((t) => t.done).length;
  return (
    <details className="todo-panel" open>
      <summary>
        План ({done}/{todos.length})
      </summary>
      <ul>
        {todos.map((t) => (
          <li key={t.id} className={t.done ? "done" : ""}>
            <span className="mark">
              {t.done ? (
                <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" strokeLinejoin="round"><path d="M20 6 9 17l-5-5" /></svg>
              ) : (
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="1.8"><circle cx="12" cy="12" r="8.5" /></svg>
              )}
            </span>
            <span className="txt">{t.text}</span>
          </li>
        ))}
      </ul>
    </details>
  );
}
