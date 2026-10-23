// Projects IPC: CRUD, ordering, archive and the per-project kanban/files
// settings.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ProjectDto } from "../types";

export const listProjects = () => invoke<ProjectDto[]>("list_projects");

export const createProject = (name: string) => invoke<ProjectDto>("create_project", { name });

export const renameProject = (id: number, newName: string) =>
  invoke<ProjectDto>("rename_project", { id, newName });

export const projectPath = (id: number) => invoke<string>("project_path", { id });

export const touchProjectOpened = (id: number) =>
  invoke<void>("touch_project_opened", { id });

export const moveProject = (id: number, position: number) =>
  invoke<void>("move_project", { id, position });

// Per-project kanban column names (exactly 3); emits "projects-changed".
export const setTaskColumns = (projectId: number, columns: string[]) =>
  invoke<ProjectDto>("set_task_columns", { projectId, columns });

// Per-project file view mode (SPEC 4.2); emits "projects-changed" so the
// store refreshes and the mode survives project switches and restarts.
export const projectSetFilesView = (projectId: number, view: "list" | "grid") =>
  invoke<ProjectDto>("project_set_files_view", { projectId, view });

export const projectSetArchived = (projectId: number, archived: boolean) =>
  invoke<ProjectDto>("project_set_archived", { projectId, archived });

export const deleteProject = (id: number) => invoke<void>("delete_project", { id });

// Reusable templates (Settings/config/templates.json, seeded on first use):
// the create command instantiates one template as a project. Note templates
// were removed (Q-15); project/task/document templates stay.
export const templateCreateProject = (templateName: string, projectName?: string) =>
  invoke<ProjectDto>("template_create_project", {
    templateName,
    projectName: projectName ?? null,
  });

export const onProjectsChanged = (handler: () => void) =>
  listen<unknown>("projects-changed", () => handler());
