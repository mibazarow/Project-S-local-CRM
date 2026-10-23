// AppStore reducer and initial state (internal to src/state/appStore).
import type { Action, AppState } from "./types";

export const initialState: AppState = {
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
  rootNavProjectId: null,
  pendingAsk: null,
  settingsRequested: false,
  newProjectRequested: false,
  newNoteRequested: false,
};

export function reducer(state: AppState, action: Action): AppState {
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
    case "rootNav":
      return { ...state, rootNavProjectId: action.id };
    case "pendingAsk":
      return { ...state, pendingAsk: action.ask };
    case "settingsRequested":
      return { ...state, settingsRequested: action.value };
    case "newProjectRequested":
      return { ...state, newProjectRequested: action.value };
    case "newNoteRequested":
      return { ...state, newNoteRequested: action.value };
  }
}
