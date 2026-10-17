import { useState } from "react";
import { useApp } from "../../state/AppStore";
import ProjectItem from "./ProjectItem";

export default function ProjectList() {
  const { state, actions } = useApp();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState("");

  const submitAdd = async () => {
    const name = draft.trim();
    setAdding(false);
    setDraft("");
    if (name) await actions.createProject(name);
  };

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
      <div className="project-list">
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
        {state.projects.map((p) => (
          <ProjectItem
            key={p.id}
            project={p}
            active={p.id === state.activeProjectId}
            onOpen={(id) => void actions.openProject(id)}
            onRename={(id, name) => void actions.renameProject(id, name)}
          />
        ))}
        {!adding && state.projects.length === 0 && (
          <div className="empty-note">Пока нет проектов. Нажми «+», чтобы создать.</div>
        )}
      </div>
    </aside>
  );
}
