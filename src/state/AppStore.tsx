import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  type ReactNode,
} from "react";
import * as ipc from "../api/ipc";
import type { FsChanged, OnboardingStatus, ProjectDto, TasksChanged } from "../api/types";

interface AppState {
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
  settingsRequested: boolean;
  newProjectRequested: boolean;
  newNoteRequested: boolean;
}

type Action =
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
  | { type: "settingsRequested"; value: boolean }
  | { type: "newProjectRequested"; value: boolean }
  | { type: "newNoteRequested"; value: boolean };

const initialState: AppState = {
  onboarding: { configured: false, workingDir: null },
  projects: [],
  activeProjectId: null,
  loading: true,
  error: null,
  treeVersion: 0,
  tasksVersion: 0,
  schedulesVersion: 0,
  pendingNotePath: null,
  pendingSessionId: null,
  pendingFilePath: null,
  pendingTaskId: null,
  settingsRequested: false,
  newProjectRequested: false,
  newNoteRequested: false,
};

function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "loading":
      return { ...state, loading: action.value };
    case "onboarding":
      return { ...state, onboarding: action.status };
    case "projects":
      return { ...state, projects: action.projects };
    case "activeProject":
      return { ...state, activeProjectId: action.id };
    case "error":
      return { ...state, error: action.message };
    case "fsChanged":
      return action.projectId === state.activeProjectId
        ? { ...state, treeVersion: state.treeVersion + 1 }
        : state;
    case "tasksChanged":
      return action.projectId === state.activeProjectId
        ? { ...state, tasksVersion: state.tasksVersion + 1 }
        : state;
    // Defensive refetch after an agent run: the run may have touched tasks
    // without a matching tasks-changed event (or the event raced the run end).
    case "tasksChangedForced":
      return { ...state, tasksVersion: state.tasksVersion + 1 };
    case "schedulesChanged":
      return { ...state, schedulesVersion: state.schedulesVersion + 1 };
    case "pendingNote":
      return { ...state, pendingNotePath: action.path };
    case "pendingSession":
      return { ...state, pendingSessionId: action.id };
    case "pendingFile":
      return { ...state, pendingFilePath: action.path };
    case "pendingTask":
      return { ...state, pendingTaskId: action.id };
    case "settingsRequested":
      return { ...state, settingsRequested: action.value };
    case "newProjectRequested":
      return { ...state, newProjectRequested: action.value };
    case "newNoteRequested":
      return { ...state, newNoteRequested: action.value };
  }
}

// The backend rejects with { code, message }; normalize to a display string.
function toMessage(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    const m = (e as { message?: unknown }).message;
    if (typeof m === "string" && m) return m;
  }
  return typeof e === "string" ? e : "Неизвестная ошибка";
}

// Last opened project per working dir: survives restarts and tab switches,
// separate key per workspace so project ids never leak across working dirs.
const lastProjectKey = (workingDir: string) =>
  `project-s:lastProject:${encodeURIComponent(workingDir)}`;

function readLastProjectId(workingDir: string | null): number | null {
  if (!workingDir) return null;
  try {
    const raw = localStorage.getItem(lastProjectKey(workingDir));
    const id = raw === null ? NaN : Number(raw);
    return Number.isInteger(id) ? id : null;
  } catch {
    return null;
  }
}

function writeLastProjectId(workingDir: string | null, id: number): void {
  if (!workingDir) return;
  try {
    localStorage.setItem(lastProjectKey(workingDir), String(id));
  } catch {
    // Storage unavailable: the selection simply won't persist across restarts.
  }
}

interface AppActions {
  chooseWorkingDir: (path: string) => Promise<void>;
  createProject: (name: string) => Promise<void>;
  createProjectFromTemplate: (templateName: string, projectName?: string) => Promise<void>;
  renameProject: (id: number, newName: string) => Promise<void>;
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
  requestSettings: () => void;
  requestNewProject: () => void;
  requestNewNote: () => void;
  clearPendingNote: () => void;
  clearPendingSession: () => void;
  clearPendingFile: () => void;
  clearPendingTask: () => void;
  clearSettingsRequested: () => void;
  clearNewProject: () => void;
  clearNewNote: () => void;
}

interface AppContextValue {
  state: AppState;
  actions: AppActions;
}

const AppContext = createContext<AppContextValue | null>(null);

export function AppProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialState);
  const configuredRef = useRef(false);

  const loadProjects = useCallback(async () => {
    if (!configuredRef.current) return;
    try {
      dispatch({ type: "projects", projects: await ipc.listProjects() });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const boot = useCallback(async () => {
    dispatch({ type: "loading", value: true });
    try {
      const status = await ipc.onboardingStatus();
      dispatch({ type: "onboarding", status });
      if (status.configured) {
        configuredRef.current = true;
        dispatch({ type: "projects", projects: await ipc.listProjects() });
      }
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    } finally {
      dispatch({ type: "loading", value: false });
    }
  }, []);

  const chooseWorkingDir = useCallback(async (path: string) => {
    dispatch({ type: "error", message: null });
    try {
      const info = await ipc.chooseWorkingDir(path);
      configuredRef.current = true;
      dispatch({ type: "onboarding", status: { configured: true, workingDir: info.workingDir } });
      // New workspace: drop the old selection and list together so stale
      // project ids never leak into the new working dir's key.
      dispatch({ type: "activeProject", id: null });
      dispatch({ type: "projects", projects: [] });
      dispatch({ type: "projects", projects: await ipc.listProjects() });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const createProject = useCallback(async (name: string) => {
    const trimmed = name.trim();
    if (!trimmed) return;
    dispatch({ type: "error", message: null });
    try {
      const dto = await ipc.createProject(trimmed);
      await ipc.touchProjectOpened(dto.id);
      configuredRef.current = true;
      dispatch({ type: "projects", projects: await ipc.listProjects() });
      dispatch({ type: "activeProject", id: dto.id });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const createProjectFromTemplate = useCallback(
    async (templateName: string, projectName?: string) => {
      dispatch({ type: "error", message: null });
      try {
        const dto = await ipc.templateCreateProject(templateName, projectName);
        await ipc.touchProjectOpened(dto.id);
        configuredRef.current = true;
        dispatch({ type: "projects", projects: await ipc.listProjects() });
        dispatch({ type: "activeProject", id: dto.id });
      } catch (e) {
        dispatch({ type: "error", message: toMessage(e) });
      }
    },
    [],
  );

  const renameProject = useCallback(async (id: number, newName: string) => {
    const trimmed = newName.trim();
    if (!trimmed) return;
    dispatch({ type: "error", message: null });
    try {
      await ipc.renameProject(id, trimmed);
      dispatch({ type: "projects", projects: await ipc.listProjects() });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const openProject = useCallback(async (id: number) => {
    dispatch({ type: "activeProject", id });
    try {
      await ipc.touchProjectOpened(id);
      dispatch({ type: "projects", projects: await ipc.listProjects() });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const moveProject = useCallback(async (id: number, position: number) => {
    dispatch({ type: "error", message: null });
    try {
      await ipc.moveProject(id, position);
      dispatch({ type: "projects", projects: await ipc.listProjects() });
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  const requestNote = useCallback((relPath: string) => dispatch({ type: "pendingNote", path: relPath }), []);
  const notifyTasksChanged = useCallback(() => dispatch({ type: "tasksChangedForced" }), []);
  const requestSession = useCallback((id: number) => dispatch({ type: "pendingSession", id }), []);
  const requestFile = useCallback((relPath: string) => dispatch({ type: "pendingFile", path: relPath }), []);
  const requestTask = useCallback((id: number) => dispatch({ type: "pendingTask", id }), []);
  const requestSettings = useCallback(() => dispatch({ type: "settingsRequested", value: true }), []);
  const requestNewProject = useCallback(() => dispatch({ type: "newProjectRequested", value: true }), []);
  const requestNewNote = useCallback(() => dispatch({ type: "newNoteRequested", value: true }), []);
  const clearPendingNote = useCallback(() => dispatch({ type: "pendingNote", path: null }), []);
  const clearPendingSession = useCallback(() => dispatch({ type: "pendingSession", id: null }), []);
  const clearPendingFile = useCallback(() => dispatch({ type: "pendingFile", path: null }), []);
  const clearPendingTask = useCallback(() => dispatch({ type: "pendingTask", id: null }), []);
  const clearSettingsRequested = useCallback(() => dispatch({ type: "settingsRequested", value: false }), []);
  const clearNewProject = useCallback(() => dispatch({ type: "newProjectRequested", value: false }), []);
  const clearNewNote = useCallback(() => dispatch({ type: "newNoteRequested", value: false }), []);

  useEffect(() => {
    void boot();
  }, [boot]);

  // Keep a valid project selection: auto-select the remembered project for
  // the current working dir, else the top of the list; clear it when no
  // projects remain. Persists the active selection so it survives restarts.
  // Tab switches never touch activeProjectId, so the selection is stable
  // across Проекты/Харнес/Блокнот.
  useEffect(() => {
    if (!state.onboarding.configured) return;
    const { projects, activeProjectId, onboarding } = state;
    if (projects.length === 0) {
      if (activeProjectId !== null) dispatch({ type: "activeProject", id: null });
      return;
    }
    if (activeProjectId !== null && projects.some((p) => p.id === activeProjectId)) {
      writeLastProjectId(onboarding.workingDir, activeProjectId);
      return;
    }
    const remembered = readLastProjectId(onboarding.workingDir);
    const next =
      remembered !== null && projects.some((p) => p.id === remembered)
        ? remembered
        : projects[0].id;
    dispatch({ type: "activeProject", id: next });
  }, [state.projects, state.activeProjectId, state.onboarding]);

  // Refresh the project list when the backend reports fs/task changes.
  useEffect(() => {
    if (!state.onboarding.configured) return;
    let cancelled = false;
    let unFs: (() => void) | undefined;
    let unTasks: (() => void) | undefined;
    let unSchedules: (() => void) | undefined;
    let unProjects: (() => void) | undefined;
    const refresh = () => void loadProjects();
    const onFs = (payload: FsChanged) => {
      refresh();
      dispatch({ type: "fsChanged", projectId: payload.projectId });
    };
    const onTasks = (payload: TasksChanged) => {
      refresh();
      dispatch({ type: "tasksChanged", projectId: payload.projectId });
    };
    // One shared schedules subscription: every schedule mutation (commands,
    // agent tools, a fired tick) bumps the counter the Harness view follows.
    const onSchedules = () => dispatch({ type: "schedulesChanged" });
    void ipc.onFsChanged(onFs).then((u) => (cancelled ? u() : (unFs = u)));
    void ipc.onTasksChanged(onTasks).then((u) => (cancelled ? u() : (unTasks = u)));
    void ipc.onSchedulesChanged(onSchedules).then((u) => (cancelled ? u() : (unSchedules = u)));
    void ipc.onProjectsChanged(refresh).then((u) => (cancelled ? u() : (unProjects = u)));
    return () => {
      cancelled = true;
      unFs?.();
      unTasks?.();
      unSchedules?.();
      unProjects?.();
    };
  }, [state.onboarding.configured, loadProjects]);

  const actions = useMemo<AppActions>(
    () => ({
      chooseWorkingDir,
      createProject,
      createProjectFromTemplate,
      renameProject,
      openProject,
      moveProject,
      refreshProjects: loadProjects,
      notifyTasksChanged,
      requestNote,
      requestSession,
      requestFile,
      requestTask,
      requestSettings,
      requestNewProject,
      requestNewNote,
      clearPendingNote,
      clearPendingSession,
      clearPendingFile,
      clearPendingTask,
      clearSettingsRequested,
      clearNewProject,
      clearNewNote,
    }),
    [
      chooseWorkingDir,
      createProject,
      createProjectFromTemplate,
      renameProject,
      openProject,
      moveProject,
      loadProjects,
      notifyTasksChanged,
      requestNote,
      requestSession,
      requestFile,
      requestTask,
      requestSettings,
      requestNewProject,
      requestNewNote,
      clearPendingNote,
      clearPendingSession,
      clearPendingFile,
      clearPendingTask,
      clearSettingsRequested,
      clearNewProject,
      clearNewNote,
    ],
  );

  const value = useMemo(() => ({ state, actions }), [state, actions]);
  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useApp(): AppContextValue {
  const ctx = useContext(AppContext);
  if (!ctx) throw new Error("useApp must be used within AppProvider");
  return ctx;
}
