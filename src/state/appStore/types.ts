// AppStore state/action contracts (internal to src/state/appStore).
import type { OnboardingStatus, ProjectDto } from "../../api/types";

export interface AppState {
  onboarding: OnboardingStatus;
  projects: ProjectDto[];
  activeProjectId: number | null;
  loading: boolean;
  error: string | null;
  // Bumped on fs-changed for the active project; file views refetch on it.
  treeVersion: number;
  // Bumped on tasks-changed for the active project; the kanban refetches on it.
  tasksVersion: number;
  // Bumped on schedules-changed (any source); the Harness view refetches on it.
  schedulesVersion: number;
  // Cross-view navigation intents (command palette): set globally, consumed
  // and cleared by the target view even when it mounts after the request.
  pendingNotePath: string | null;
  pendingSessionId: number | null;
  // File/task targets inside the active project, consumed by FilesPanel
  // (select/preview the file; switch to «Задачи» and highlight the task).
  pendingFilePath: string | null;
  pendingTaskId: number | null;
  // Clicking the already-active project's name in the sidebar: FilesPanel
  // resets to the project root (path "") and closes the preview. Consumed and
  // cleared by FilesPanel for the matching project.
  rootNavProjectId: number | null;
  // Cross-entity quick action: a prebuilt question for the project chat,
  // consumed and cleared by ProjectChat for the matching project.
  pendingAsk: { projectId: number; text: string } | null;
  settingsRequested: boolean;
  newProjectRequested: boolean;
  newNoteRequested: boolean;
}

export type Action =
  | { type: "loading"; value: boolean }
  | { type: "onboarding"; status: OnboardingStatus }
  | { type: "projects"; projects: ProjectDto[] }
  | { type: "activeProject"; id: number | null }
  | { type: "error"; message: string | null }
  | { type: "fsChanged"; projectId: number }
  | { type: "tasksChanged"; projectId: number }
  | { type: "tasksChangedForced" }
  | { type: "schedulesChanged" }
  | { type: "pendingNote"; path: string | null }
  | { type: "pendingSession"; id: number | null }
  | { type: "pendingFile"; path: string | null }
  | { type: "pendingTask"; id: number | null }
  | { type: "rootNav"; id: number | null }
  | { type: "pendingAsk"; ask: { projectId: number; text: string } | null }
  | { type: "settingsRequested"; value: boolean }
  | { type: "newProjectRequested"; value: boolean }
  | { type: "newNoteRequested"; value: boolean };

export interface AppActions {
  chooseWorkingDir: (path: string) => Promise<void>;
  createProject: (name: string) => Promise<void>;
  createProjectFromTemplate: (templateName: string, projectName?: string) => Promise<void>;
  renameProject: (id: number, newName: string) => Promise<void>;
  archiveProject: (id: number, archived: boolean) => Promise<void>;
  deleteProject: (id: number) => Promise<boolean>;
  openProject: (id: number) => Promise<void>;
  moveProject: (id: number, position: number) => Promise<void>;
  refreshProjects: () => Promise<void>;
  // Defensive task refetch after an agent run finishes (chat views call it):
  // bumps tasksVersion regardless of the event payload's project match.
  notifyTasksChanged: () => void;
  // Command-palette navigation intents; each target view clears its own.
  requestNote: (relPath: string) => void;
  requestSession: (id: number) => void;
  requestFile: (relPath: string) => void;
  requestTask: (id: number) => void;
  // Clicking the already-active project's name: ask FilesPanel to go to root.
  requestProjectRoot: (id: number) => void;
  // Cross-entity quick action: ask the project chat a prebuilt question.
  requestAskAgent: (projectId: number, text: string) => void;
  requestSettings: () => void;
  requestNewProject: () => void;
  requestNewNote: () => void;
  clearPendingNote: () => void;
  clearPendingSession: () => void;
  clearPendingFile: () => void;
  clearPendingTask: () => void;
  clearProjectRoot: () => void;
  clearPendingAsk: () => void;
  clearSettingsRequested: () => void;
  clearNewProject: () => void;
  clearNewNote: () => void;
}

export interface AppContextValue {
  state: AppState;
  actions: AppActions;
}
