// AppProvider, contexts and the useAppState/useAppActions/useApp hooks
// (internal to src/state/appStore; re-exported by ../../AppStore).
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
import * as ipc from "../../api/ipc";
import type { FsChanged, TasksChanged } from "../../api/types";
import { toMessage } from "../../shared/errors";
import { initialState, reducer } from "./reducer";
import { readLastProjectId, writeLastProjectId } from "./storage";
import type { AppActions, AppContextValue, AppState } from "./types";

const AppStateContext = createContext<AppState | null>(null);
const AppActionsContext = createContext<AppActions | null>(null);

export function AppProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialState);
  const configuredRef = useRef(false);
  const activeProjectIdRef = useRef<number | null>(null);

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

  const archiveProject = useCallback(async (id: number, archived: boolean) => {
    dispatch({ type: "error", message: null });
    try {
      await ipc.projectSetArchived(id, archived);
      const projects = await ipc.listProjects();
      dispatch({ type: "projects", projects });
      if (archived && id === activeProjectIdRef.current) {
        const top = projects.find((p) => !p.archived);
        dispatch({ type: "activeProject", id: top ? top.id : null });
      }
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
    }
  }, []);

  // Permanent deletion (no undo): returns whether it succeeded so the list
  // can toast. If the deleted project was active, fall back to the first
  // non-archived one, same as archiving does.
  const deleteProject = useCallback(async (id: number) => {
    dispatch({ type: "error", message: null });
    try {
      await ipc.deleteProject(id);
      const projects = await ipc.listProjects();
      dispatch({ type: "projects", projects });
      if (id === activeProjectIdRef.current) {
        const top = projects.find((p) => !p.archived);
        dispatch({ type: "activeProject", id: top ? top.id : null });
      }
      return true;
    } catch (e) {
      dispatch({ type: "error", message: toMessage(e) });
      return false;
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
  const requestProjectRoot = useCallback((id: number) => dispatch({ type: "rootNav", id }), []);
  const requestAskAgent = useCallback(
    (projectId: number, text: string) => dispatch({ type: "pendingAsk", ask: { projectId, text } }),
    [],
  );
  const requestSettings = useCallback(() => dispatch({ type: "settingsRequested", value: true }), []);
  const requestNewProject = useCallback(() => dispatch({ type: "newProjectRequested", value: true }), []);
  const requestNewNote = useCallback(() => dispatch({ type: "newNoteRequested", value: true }), []);
  const clearPendingNote = useCallback(() => dispatch({ type: "pendingNote", path: null }), []);
  const clearPendingSession = useCallback(() => dispatch({ type: "pendingSession", id: null }), []);
  const clearPendingFile = useCallback(() => dispatch({ type: "pendingFile", path: null }), []);
  const clearPendingTask = useCallback(() => dispatch({ type: "pendingTask", id: null }), []);
  const clearProjectRoot = useCallback(() => dispatch({ type: "rootNav", id: null }), []);
  const clearPendingAsk = useCallback(() => dispatch({ type: "pendingAsk", ask: null }), []);
  const clearSettingsRequested = useCallback(() => dispatch({ type: "settingsRequested", value: false }), []);
  const clearNewProject = useCallback(() => dispatch({ type: "newProjectRequested", value: false }), []);
  const clearNewNote = useCallback(() => dispatch({ type: "newNoteRequested", value: false }), []);

  useEffect(() => {
    void boot();
  }, [boot]);

  useEffect(() => {
    activeProjectIdRef.current = state.activeProjectId;
  }, [state.activeProjectId]);

  // Keep a valid project selection: auto-select the remembered project for
  // the current working dir, else the top of the current (non-archived) list;
  // clear it when no current projects remain. Persists the active selection
  // so it survives restarts. An archived project stays selected only while it
  // was opened explicitly from the «Архивные» tab, never as a fallback.
  // Tab switches never touch activeProjectId, so the selection is stable
  // across Проекты/Харнес/Заметки и файлы.
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
    const current = projects.filter((p) => !p.archived);
    if (current.length === 0) return;
    const remembered = readLastProjectId(onboarding.workingDir);
    const next =
      remembered !== null && current.some((p) => p.id === remembered)
        ? remembered
        : current[0].id;
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
    let unScheduleOpen: (() => void) | undefined;
    let unChatOpen: (() => void) | undefined;
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
    // Q.md items 4/7: a click on a schedule-run notification and the creation
    // of a new schedule both route through the pendingSession intent; App
    // switches to the Harness tab and HarnessView opens the session.
    void ipc
      .onScheduleOpen((payload) => dispatch({ type: "pendingSession", id: payload.sessionId }))
      .then((u) => (cancelled ? u() : (unScheduleOpen = u)));
    // Q.md Harness chat search: the open_chat tool routes the UI to the found
    // session through the same pendingSession intent App + HarnessView consume.
    void ipc
      .onChatOpen((payload) => dispatch({ type: "pendingSession", id: payload.sessionId }))
      .then((u) => (cancelled ? u() : (unChatOpen = u)));
    return () => {
      cancelled = true;
      unFs?.();
      unTasks?.();
      unSchedules?.();
      unProjects?.();
      unScheduleOpen?.();
      unChatOpen?.();
    };
  }, [state.onboarding.configured, loadProjects]);

  const actions = useMemo<AppActions>(
    () => ({
      chooseWorkingDir,
      createProject,
      createProjectFromTemplate,
      renameProject,
      archiveProject,
      deleteProject,
      openProject,
      moveProject,
      refreshProjects: loadProjects,
      notifyTasksChanged,
      requestNote,
      requestSession,
      requestFile,
      requestTask,
      requestProjectRoot,
      requestAskAgent,
      requestSettings,
      requestNewProject,
      requestNewNote,
      clearPendingNote,
      clearPendingSession,
      clearPendingFile,
      clearPendingTask,
      clearProjectRoot,
      clearPendingAsk,
      clearSettingsRequested,
      clearNewProject,
      clearNewNote,
    }),
    [
      chooseWorkingDir,
      createProject,
      createProjectFromTemplate,
      renameProject,
      archiveProject,
      deleteProject,
      openProject,
      moveProject,
      loadProjects,
      notifyTasksChanged,
      requestNote,
      requestSession,
      requestFile,
      requestTask,
      requestProjectRoot,
      requestAskAgent,
      requestSettings,
      requestNewProject,
      requestNewNote,
      clearPendingNote,
      clearPendingSession,
      clearPendingFile,
      clearPendingTask,
      clearProjectRoot,
      clearPendingAsk,
      clearSettingsRequested,
      clearNewProject,
      clearNewNote,
    ],
  );

  return (
    <AppActionsContext.Provider value={actions}>
      <AppStateContext.Provider value={state}>{children}</AppStateContext.Provider>
    </AppActionsContext.Provider>
  );
}

export function useAppState(): AppState {
  const state = useContext(AppStateContext);
  if (!state) throw new Error("useAppState must be used within AppProvider");
  return state;
}

export function useAppActions(): AppActions {
  const actions = useContext(AppActionsContext);
  if (!actions) throw new Error("useAppActions must be used within AppProvider");
  return actions;
}

export function useApp(): AppContextValue {
  return { state: useAppState(), actions: useAppActions() };
}
