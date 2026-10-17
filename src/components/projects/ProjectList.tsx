import { useEffect, useRef, useState } from "react";
import { useApp } from "../../state/AppStore";
import ProjectItem from "./ProjectItem";

export default function ProjectList() {
  const { state, actions } = useApp();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");
  const [draggingId, setDraggingId] = useState<number | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const lastDragEndRef = useRef(0);

  // Command-palette intent: open the inline "new project" input.
  useEffect(() => {
    if (!state.newProjectRequested) return;
    actions.clearNewProject();
    setDraft("");
    setAdding(true);
  }, [state.newProjectRequested, actions]);

  const submitAdd = async () => {
    const name = draft.trim();
    setAdding(false);
    setDraft("");
    if (name) await actions.createProject(name);
  };

  // Insertion index among the non-dragged items, by pointer Y vs item midlines.
  const insertIndexAt = (clientY: number): number => {
    const items = Array.from(
      listRef.current?.querySelectorAll<HTMLElement>(".project-item[data-id]") ?? [],
    ).filter((el) => Number(el.dataset.id) !== draggingId);
    for (let i = 0; i < items.length; i++) {
      const rect = items[i].getBoundingClientRect();
      if (clientY < rect.top + rect.height / 2) return i;
    }
    return items.length;
  };

  const handleDragOver = (e: React.DragEvent<HTMLDivElement>) => {
    if (draggingId === null) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    setDropIndex(insertIndexAt(e.clientY));
  };

  const handleDragLeave = (e: React.DragEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDropIndex(null);
  };

  const handleDrop = (e: React.DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    setDropIndex(null);
    const id = Number(e.dataTransfer.getData("text/plain"));
    if (!Number.isFinite(id)) return;
    // Same fractional scheme as the kanban: midpoint between neighbors,
    // first-1 for the top, last+1 for the end.
    const others = state.projects.filter((p) => p.id !== id);
    const index = insertIndexAt(e.clientY);
    let position: number;
    if (others.length === 0) position = 1;
    else if (index === 0) position = others[0].position - 1;
    else if (index >= others.length) position = others[others.length - 1].position + 1;
    else position = (others[index - 1].position + others[index].position) / 2;
    void actions.moveProject(id, position);
  };

  const visible = state.projects.filter((p) => p.id !== draggingId);
  const lastVisibleId = visible.length ? visible[visible.length - 1].id : null;

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span className="sidebar-title">Проекты</span>
        <button
          className="icon-btn small"
          title="Добавить проект"
          onClick={() => {
            setDraft("");
            setAdding(true);
          }}
        >
          <svg
            viewBox="0 0 24 24"
            width="16"
            height="16"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
          >
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>
      </div>
      <div
        ref={listRef}
        className="project-list"
        onDragOver={handleDragOver}
        onDragLeave={handleDragLeave}
        onDrop={handleDrop}
      >
        {adding && (
          <input
            className="rename-input add-input"
            placeholder="Имя проекта…"
            value={draft}
            autoFocus
            onChange={(e) => setDraft(e.target.value)}
            onBlur={() => void submitAdd()}
            onKeyDown={(e) => {
              if (e.key === "Enter") void submitAdd();
              if (e.key === "Escape") {
                setAdding(false);
                setDraft("");
              }
            }}
          />
        )}
        {state.projects.map((p) => {
          let dropCls = "";
          if (draggingId !== null && dropIndex !== null && p.id !== draggingId) {
            if (dropIndex < visible.length && visible[dropIndex].id === p.id) {
              dropCls = "drop-before";
            } else if (dropIndex === visible.length && p.id === lastVisibleId) {
              dropCls = "drop-after";
            }
          }
          return (
            <ProjectItem
              key={p.id}
              project={p}
              active={p.id === state.activeProjectId}
              dragging={p.id === draggingId}
              dropCls={dropCls}
              onOpen={(id) => {
                // A drag must not open the project it ended on.
                if (Date.now() - lastDragEndRef.current < 250) return;
                void actions.openProject(id);
              }}
              onRename={(id, name) => void actions.renameProject(id, name)}
              onDragStart={(e) => {
                e.dataTransfer.setData("text/plain", String(p.id));
                e.dataTransfer.effectAllowed = "move";
                setDraggingId(p.id);
              }}
              onDragEnd={() => {
                lastDragEndRef.current = Date.now();
                setDraggingId(null);
                setDropIndex(null);
              }}
            />
          );
        })}
        {!adding && state.projects.length === 0 && (
          <div className="empty-note">Пока нет проектов. Нажми «+», чтобы создать.</div>
        )}
      </div>
    </aside>
  );
}
