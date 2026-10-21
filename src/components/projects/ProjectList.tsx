import { useEffect, useRef, useState, type MouseEvent, type ReactNode } from "react";
import { useApp } from "../../state/AppStore";
import { usePointerDrag, type DropTarget } from "../../hooks/usePointerDrag";
import { confirmRemove } from "../../shared/confirm";
import ContextMenu from "../shared/ContextMenu";
import Segmented from "../shared/Segmented";
import { toast } from "../shared/toast";
import ProjectItem from "./ProjectItem";

export default function ProjectList() {
  const { state, actions } = useApp();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");
  const [filter, setFilter] = useState<"current" | "archived">("current");
  const [menu, setMenu] = useState<{ id: number; x: number; y: number } | null>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const visible = state.projects.filter((p) => p.archived === (filter === "archived"));

  // Command-palette intent: open the inline "new project" input.
  useEffect(() => {
    if (!state.newProjectRequested) return;
    actions.clearNewProject();
    setDraft("");
    setFilter("current");
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
    resolveTarget: (clientX, clientY, id): DropTarget<number> | null => {
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
      return { container: "list", index, draggingId: id };
    },
    // Midpoint between neighbors, first-1 for the top, last+1 for the end.
    onDrop: (id, target) => {
      const others = visible.filter((p) => p.id !== id);
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
    // Already-active project: clicking its name acts like the home crumb and
    // returns the file view to the project root instead of re-opening it.
    if (id === state.activeProjectId) {
      actions.requestProjectRoot(id);
      return;
    }
    void actions.openProject(id);
  };

  const handleArchive = (id: number, archived: boolean) => {
    void actions.archiveProject(id, archived);
    if (archived && id === state.activeProjectId) setFilter("current");
  };

  // Permanent deletion: the confirm spells out that nothing is recoverable
  // (no Trash, no undo), matching the backend that wipes DB rows and folder.
  const handleDelete = async (id: number) => {
    const project = state.projects.find((p) => p.id === id);
    if (!project) return;
    const ok = await confirmRemove({
      title: `Удалить проект «${project.name}»?`,
      message:
        "Проект и его папка на диске удаляются безвозвратно: задачи, чаты, память проекта, файлы и история изменений. Корзина и восстановление не предусмотрены.",
      confirmLabel: "Удалить навсегда",
    });
    if (!ok) return;
    if (await actions.deleteProject(id)) {
      toast("ok", `Проект «${project.name}» удалён`);
      if (id === state.activeProjectId) setFilter("current");
    }
  };

  const handleContextMenu = (e: MouseEvent<HTMLDivElement>, id: number) => {
    e.preventDefault();
    const project = state.projects.find((p) => p.id === id);
    if (!project || project.builtin) return;
    setMenu({ id, x: e.clientX, y: e.clientY });
  };

  const dropIndex = dropTarget ? dropTarget.index : null;
  const rows: ReactNode[] = [];
  let visibleIndex = 0;
  for (const p of visible) {
    if (p.id === draggingId) {
      rows.push(
        <ProjectItem
          key={p.id}
          project={p}
          active={p.id === state.activeProjectId}
          dragging
          onOpen={handleOpen}
          onRename={(id, name) => void actions.renameProject(id, name)}
          onArchive={handleArchive}
          onDelete={(id) => void handleDelete(id)}
          onContextMenu={(e) => handleContextMenu(e, p.id)}
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
        onArchive={handleArchive}
        onDelete={(id) => void handleDelete(id)}
        onContextMenu={(e) => handleContextMenu(e, p.id)}
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
        <span className="sidebar-head-actions">
          <button
            className="add-project-btn"
            title="Новый проект"
            onClick={() => {
              setDraft("");
              setFilter("current");
              setAdding(true);
            }}
          >
            <span className="add-project-label">Новый проект</span>
            <span className="add-project-plus" aria-hidden="true">
              <svg
                viewBox="0 0 24 24"
                width="14"
                height="14"
                fill="none"
                stroke="currentColor"
                strokeWidth="2.4"
                strokeLinecap="round"
              >
                <path d="M12 5v14M5 12h14" />
              </svg>
            </span>
          </button>
        </span>
      </div>
      <div className="sidebar-tabs">
        <Segmented
          value={filter}
          options={[
            { value: "current", label: "Текущие" },
            { value: "archived", label: "Архивные" },
          ]}
          onChange={setFilter}
          wrapperClassName="segmented sidebar-seg"
        />
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
        {!adding && visible.length === 0 && (
          <div className="empty-note">
            {filter === "archived"
              ? "Архивных проектов пока нет."
              : "Пока нет проектов. Нажми «+», чтобы создать."}
          </div>
        )}
      </div>
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={[{ label: "Удалить проект…", onSelect: () => void handleDelete(menu.id) }]}
          onClose={() => setMenu(null)}
        />
      )}
    </aside>
  );
}
