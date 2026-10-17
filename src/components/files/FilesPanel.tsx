import { useCallback, useEffect, useRef, useState } from "react";
import type { FsEntry, ProjectDto } from "../../api/types";
import { useApp } from "../../state/AppStore";
import Breadcrumbs from "./Breadcrumbs";
import FileGrid from "./FileGrid";
import FilePreview from "./FilePreview";
import FileTree from "./FileTree";
import SearchResults from "./SearchResults";
import { IconClose, IconGrid, IconList, IconSearch } from "./icons";
import KanbanBoard from "../tasks/KanbanBoard";

interface Props {
  project: ProjectDto;
}

export default function FilesPanel({ project }: Props) {
  const { state } = useApp();
  const [tab, setTab] = useState<"files" | "tasks">("files");
  const [view, setView] = useState<"list" | "grid">("list");
  const [selected, setSelected] = useState<FsEntry | null>(null);
  const [query, setQuery] = useState("");
  const [activeQuery, setActiveQuery] = useState("");
  const [highlightTaskId, setHighlightTaskId] = useState<number | null>(null);
  const dirtyRef = useRef(false);
  const setDirty = useCallback((dirty: boolean) => {
    dirtyRef.current = dirty;
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => setActiveQuery(query.trim()), 250);
    return () => clearTimeout(timer);
  }, [query]);

  useEffect(() => {
    if (highlightTaskId === null) return;
    const timer = setTimeout(() => setHighlightTaskId(null), 2000);
    return () => clearTimeout(timer);
  }, [highlightTaskId]);

  const confirmDiscard = () =>
    !dirtyRef.current ||
    window.confirm("Есть несохранённые изменения. Отменить их?");

  const handleSelect = (entry: FsEntry) => {
    if (entry.relPath !== selected?.relPath && !confirmDiscard()) return;
    setSelected(entry);
  };

  const handleClosePreview = () => {
    if (!confirmDiscard()) return;
    setSelected(null);
  };

  const switchTab = (next: "files" | "tasks") => {
    if (next !== tab && !confirmDiscard()) return;
    setTab(next);
    setHighlightTaskId(null);
  };

  // SearchResults only renders on the files tab, so this always switches tabs.
  const openTaskFromSearch = (taskId: number | null) => {
    if (!confirmDiscard()) return;
    setTab("tasks");
    setHighlightTaskId(taskId);
  };

  return (
    <section className="center">
      <div className="center-head">
        <h2>{project.name}</h2>
        <div className="segmented">
          <button
            className={tab === "files" ? "seg active" : "seg"}
            onClick={() => switchTab("files")}
          >
            Файлы
          </button>
          <button
            className={tab === "tasks" ? "seg active" : "seg"}
            onClick={() => switchTab("tasks")}
          >
            Задачи
          </button>
        </div>
        {tab === "files" && (
          <>
            <div className="view-toggle" title="Вид файлов">
              <button
                className={view === "list" ? "vt active" : "vt"}
                title="Список"
                onClick={() => setView("list")}
              >
                <IconList />
              </button>
              <button
                className={view === "grid" ? "vt active" : "vt"}
                title="Сетка"
                onClick={() => setView("grid")}
              >
                <IconGrid />
              </button>
            </div>
            <div className="search">
              <IconSearch />
              <input
                type="text"
                placeholder="Поиск по проекту…"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
              {query && (
                <button
                  className="icon-btn small"
                  title="Очистить поиск"
                  onClick={() => setQuery("")}
                >
                  <IconClose />
                </button>
              )}
            </div>
          </>
        )}
      </div>

      {tab === "tasks" ? (
        <KanbanBoard
          projectId={project.id}
          tasksVersion={state.tasksVersion}
          highlightTaskId={highlightTaskId}
        />
      ) : (
        <>
          <Breadcrumbs projectName={project.name} selected={selected} />

          <div className="center-body">
            {activeQuery ? (
              <SearchResults
                projectId={project.id}
                query={activeQuery}
                onSelect={handleSelect}
                onOpenTask={openTaskFromSearch}
              />
            ) : view === "list" ? (
              <FileTree
                projectId={project.id}
                treeVersion={state.treeVersion}
                selectedRelPath={selected?.relPath ?? null}
                selectedDirRel={selected && selected.kind === "dir" ? selected.relPath : ""}
                onSelect={handleSelect}
              />
            ) : (
              <FileGrid
                projectId={project.id}
                treeVersion={state.treeVersion}
                selectedRelPath={selected?.relPath ?? null}
                onSelect={handleSelect}
              />
            )}
            {selected && selected.kind === "file" && (
              <FilePreview
                key={selected.relPath}
                projectId={project.id}
                entry={selected}
                onClose={handleClosePreview}
                onDirtyChange={setDirty}
              />
            )}
          </div>
        </>
      )}
    </section>
  );
}
