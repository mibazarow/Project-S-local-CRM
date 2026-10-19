import { useCallback, useEffect, useRef, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, ProjectDto } from "../../api/types";
import { useApp } from "../../state/AppStore";
import { resolveEntry } from "../../shared/resolveEntry";
import { confirmAction } from "../../shared/confirm";
import Breadcrumbs from "./Breadcrumbs";
import FileGrid from "./FileGrid";
import FilePreview from "./FilePreview";
import FileTree from "./FileTree";
import KanbanBoard from "../tasks/KanbanBoard";
import Segmented from "../shared/Segmented";

interface Props {
  project: ProjectDto;
}

export default function FilesPanel({ project }: Props) {
  const { state, actions } = useApp();
  const [tab, setTab] = useState<"files" | "tasks">("files");
  // Per-project file view mode (SPEC 4.2): hydrated from the project DTO;
  // the panel is keyed by project id in App, so switching projects remounts
  // and restores each project's remembered mode (no reset to "list").
  const [view, setView] = useState<"list" | "grid">(project.filesView);
  // Grid navigation: the folder currently shown by the grid ("" = project root).
  const [currentDir, setCurrentDir] = useState("");
  const [selected, setSelected] = useState<FsEntry | null>(null);
  const [highlightTaskId, setHighlightTaskId] = useState<number | null>(null);
  const [overdueOnly, setOverdueOnly] = useState(false);
  const dirtyRef = useRef(false);
  const setDirty = useCallback((dirty: boolean) => {
    dirtyRef.current = dirty;
  }, []);

  useEffect(() => {
    if (highlightTaskId === null) return;
    const timer = setTimeout(() => setHighlightTaskId(null), 2000);
    return () => clearTimeout(timer);
  }, [highlightTaskId]);

  useEffect(() => {
    if (project.archived && tab === "tasks") setTab("files");
  }, [project.archived, tab]);

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
    void (async () => {
      if (!(await confirmDiscard())) return;
      try {
        const entry = await resolveEntry(path, (dir) => ipc.fsListDir(project.id, dir));
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
    void (async () => {
      if (!(await confirmDiscard())) return;
      setTab("tasks");
      setHighlightTaskId(id);
    })();
  }, [state.pendingTaskId, actions]);

  const confirmDiscard = async () =>
    !dirtyRef.current ||
    (await confirmAction({
      title: "Отменить несохранённые изменения?",
      message: "В редакторе есть несохранённые изменения — они будут потеряны.",
      confirmLabel: "Отменить изменения",
      danger: true,
    }));

  const handleSelect = async (entry: FsEntry) => {
    if (entry.relPath !== selected?.relPath && !(await confirmDiscard())) return;
    setSelected(entry);
  };

  const handleClosePreview = async () => {
    if (!(await confirmDiscard())) return;
    setSelected(null);
  };

  const switchTab = async (next: "files" | "tasks") => {
    if (next !== tab && !(await confirmDiscard())) return;
    setTab(next);
    setHighlightTaskId(null);
  };

  // Local state flips immediately; the backend persists the mode per project
  // and emits "projects-changed", so AppStore refreshes the project list and
  // the saved value arrives back through the `project` prop (restart path).
  const changeView = (next: "list" | "grid") => {
    setView(next);
    void ipc.projectSetFilesView(project.id, next).catch(() => {
      // Persistence failed: the local mode stays valid until the next mount.
    });
  };

  return (
    <section className="center">
      <div className="center-head">
        <h2>{project.name}</h2>
        <Segmented
          value={tab}
          options={[
            { value: "files", label: "Файлы" },
            ...(!project.archived ? [{ value: "tasks" as const, label: "Задачи" }] : []),
          ]}
          onChange={switchTab}
        />
        {tab === "tasks" && (
          <label className={overdueOnly ? "seg-filter active" : "seg-filter"}>
            <input
              type="checkbox"
              checked={overdueOnly}
              onChange={(e) => setOverdueOnly(e.target.checked)}
            />
            Просроченные
          </label>
        )}
      </div>

      {tab === "tasks" ? (
        <KanbanBoard
          project={project}
          tasksVersion={state.tasksVersion}
          highlightTaskId={highlightTaskId}
          overdueOnly={overdueOnly}
        />
      ) : (
        <>
          <Breadcrumbs
            path={view === "grid" ? currentDir : (selected?.relPath ?? "")}
            onNavigate={view === "grid" ? setCurrentDir : undefined}
          />

          <div className="center-body">
            {view === "list" ? (
              <FileTree
                projectId={project.id}
                treeVersion={state.treeVersion}
                selectedRelPath={selected?.relPath ?? null}
                selectedDirRel={selected && selected.kind === "dir" ? selected.relPath : ""}
                onSelect={handleSelect}
                view={view}
                onViewChange={changeView}
              />
            ) : (
              <FileGrid
                projectId={project.id}
                treeVersion={state.treeVersion}
                currentDir={currentDir}
                selectedRelPath={selected?.relPath ?? null}
                onSelect={handleSelect}
                onNavigateDir={setCurrentDir}
                view={view}
                onViewChange={changeView}
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
