import { useEffect, useRef, useState, type ReactNode } from "react";
import { useApp } from "../../state/AppStore";
import { usePointerDrag } from "../../hooks/usePointerDrag";
import ProjectItem from "./ProjectItem";

export default function ProjectList() {
  const { state, actions } = useApp();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");
  const listRef = useRef<HTMLDivElement>(null);

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

  const { draggingId, dropTarget, onPointerDown, consumeSuppressedClick } = usePointerDrag({
    // Insertion index among the non-dragged items, by pointer Y vs item midlines.
    resolveTarget: (clientX, clientY, id) => {
      const list = listRef.current;
      if (!list) return null;
      const bounds = list.getBoundingClientRect();
      if (
        clientX < bounds.left ||
        clientX > bounds.right ||
        clientY < bounds.top ||
        clientY > bounds.bottom
      ) {
        return null;
      }
      const items = Array.from(
        list.querySelectorAll<HTMLElement>(".project-item[data-id]"),
      ).filter((el) => Number(el.dataset.id) !== id);
      let index = items.length;
      for (let i = 0; i < items.length; i++) {
        const rect = items[i].getBoundingClientRect();
        if (clientY < rect.top + rect.height / 2) {
          index = i;
          break;
        }
      }
      return { container: "list", index };
    },
    // Midpoint between neighbors, first-1 for the top, last+1 for the end.
    onDrop: (id, target) => {
      const others = state.projects.filter((p) => p.id !== id);
      let position: number;
      if (others.length === 0) position = 1;
      else if (target.index === 0) position = others[0].position - 1;
      else if (target.index >= others.length) position = others[others.length - 1].position + 1;
      else position = (others[target.index - 1].position + others[target.index].position) / 2;
      void actions.moveProject(id, position);
    },
  });

  const handleOpen = (id: number) => {
    // A drag must not open the project it ended on.
    if (consumeSuppressedClick()) return;
    void actions.openProject(id);
  };

  const dropIndex = dropTarget ? dropTarget.index : null;
  const rows: ReactNode[] = [];
  let visibleIndex = 0;
  for (const p of state.projects) {
    if (p.id === draggingId) {
      rows.push(
        <ProjectItem
          key={p.id}
          project={p}
          active={p.id === state.activeProjectId}
          dragging
          onOpen={handleOpen}
          onRename={(id, name) => void actions.renameProject(id, name)}
          onPointerDown={(e) => onPointerDown(e, p.id)}
        />,
      );
      continue;
    }
    if (dropIndex !== null && dropIndex === visibleIndex) {
      rows.push(<div key="drop-indicator" className="drop-indicator" />);
    }
    rows.push(
      <ProjectItem
        key={p.id}
        project={p}
        active={p.id === state.activeProjectId}
        dragging={false}
        onOpen={handleOpen}
        onRename={(id, name) => void actions.renameProject(id, name)}
        onPointerDown={(e) => onPointerDown(e, p.id)}
      />,
    );
    visibleIndex++;
  }
  if (dropIndex !== null && dropIndex === visibleIndex) {
    rows.push(<div key="drop-indicator" className="drop-indicator" />);
  }

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
      <div ref={listRef} className="project-list">
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
        {rows}
        {!adding && state.projects.length === 0 && (
          <div className="empty-note">Пока нет проектов. Нажми «+», чтобы создать.</div>
        )}
      </div>
    </aside>
  );
}
