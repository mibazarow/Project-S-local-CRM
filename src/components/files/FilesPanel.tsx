import { useCallback, useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
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
  const { state, actions } = useApp();
  const [tab, setTab] = useState<"files" | "tasks">("files");
  const [view, setView] = useState<"list" | "grid">("list");
  // Grid navigation: the folder currently shown by the grid ("" = project root).
  const [currentDir, setCurrentDir] = useState("");
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

  // Grid navigation is per-project: an entered folder must not leak across projects.
  useEffect(() => {
    setCurrentDir("");
  }, [project.id]);

  // Command-palette intent: open a file by project-relative path (resolved to
  // a real FsEntry through the folder listing, like the search results do).
  // Runs on mount too, so the intent survives a project switch made together
  // with the palette click.
  useEffect(() => {
    const path = state.pendingFilePath;
    if (!path) return;
    actions.clearPendingFile();
    if (!confirmDiscard()) return;
    void (async () => {
      const slash = path.lastIndexOf("/");
      const dir = slash === -1 ? "" : path.slice(0, slash);
      try {
        const entries = await ipc.fsListDir(project.id, dir);
        const entry = entries.find((e) => e.relPath === path);
        if (entry) {
          setTab("files");
          setSelected(entry);
        }
      } catch {
        // The file vanished between palette load and open: stay put.
      }
    })();
  }, [state.pendingFilePath, actions, project.id]);

  // Command-palette intent: switch to «Задачи» and highlight the task card.
  useEffect(() => {
    const id = state.pendingTaskId;
    if (id == null) return;
    actions.clearPendingTask();
    if (!confirmDiscard()) return;
    setTab("tasks");
    setHighlightTaskId(id);
  }, [state.pendingTaskId, actions]);

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
        {project.overdueCount > 0 && (
          <span className="badge st-overdue">{project.overdueCount} просрочено</span>
        )}
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
          project={project}
          tasksVersion={state.tasksVersion}
          highlightTaskId={highlightTaskId}
        />
      ) : (
        <>
          <Breadcrumbs
            projectName={project.name}
            path={view === "grid" ? currentDir : (selected?.relPath ?? "")}
            onNavigate={view === "grid" ? setCurrentDir : undefined}
          />

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
                currentDir={currentDir}
                selectedRelPath={selected?.relPath ?? null}
                onSelect={handleSelect}
                onNavigateDir={setCurrentDir}
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
