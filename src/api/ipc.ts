import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AgentEventPayload,
  AttachmentDto,
  BackupInfo,
  ConfigChanged,
  ConfigKind,
  ConnectionTest,
  EmbedStatus,
  FsChanged,
  FsEntry,
  IndexChanged,
  ImportResult,
  JournalListResult,
  McpServerStatus,
  MemoryChanged,
  MemoryDto,
  MessageAttachmentDto,
  MessageDto,
  ModelInfo,
  OnboardingStatus,
  ConfirmationRequestPayload,
  ProjectDto,
  ProjectIndexStats,
  ScheduleDto,
  SearchHit,
  SessionDto,
  SkillInfo,
  TaskDto,
  TaskPatch,
  TasksChanged,
  TmpStats,
  VisionStatus,
  WorkingDirInfo,
} from "./types";

// One typed wrapper per IPC command. JSX must not call invoke() directly.

export const onboardingStatus = () => invoke<OnboardingStatus>("onboarding_status");

export const defaultWorkingDir = () => invoke<string>("default_working_dir");

export const chooseWorkingDir = (path: string) =>
  invoke<WorkingDirInfo>("choose_working_dir", { path });

export const getWorkingDir = () => invoke<string | null>("get_working_dir");

export const configDirPath = () => invoke<string>("config_dir_path");

export const readConfig = (kind: ConfigKind) => invoke<unknown>("read_config", { kind });

export const writeConfig = (kind: ConfigKind, value: unknown) =>
  invoke<void>("write_config", { kind, value });

// Ops (SPEC 3.6 backup, 5.2 tmp hygiene, 5.3 autostart).
export const clearTmp = () => invoke<TmpStats>("clear_tmp");

export const createBackup = (dest?: string) =>
  invoke<BackupInfo>("create_backup", { dest: dest ?? null });

export const backupsDir = () => invoke<string>("backups_dir");

export const getAutostart = () => invoke<boolean>("get_autostart");

export const setAutostart = (enabled: boolean) => invoke<boolean>("set_autostart", { enabled });

// Run journal (SPEC 3.3): read-only feed over Settings/logs/runs-*.jsonl.
// Empty-string filters are sent as null (no filter).
export interface JournalQuery {
  days?: number;
  kind?: string;
  tool?: string;
  ok?: boolean;
  projectId?: number;
  sessionId?: number;
  limit?: number;
}

export const journalList = (q: JournalQuery) =>
  invoke<JournalListResult>("journal_list", {
    days: q.days ?? null,
    kind: q.kind || null,
    tool: q.tool || null,
    ok: q.ok ?? null,
    projectId: q.projectId ?? null,
    sessionId: q.sessionId ?? null,
    limit: q.limit ?? null,
  });

export const listProjects = () => invoke<ProjectDto[]>("list_projects");

export const createProject = (name: string) => invoke<ProjectDto>("create_project", { name });

export const renameProject = (id: number, newName: string) =>
  invoke<ProjectDto>("rename_project", { id, newName });

export const projectPath = (id: number) => invoke<string>("project_path", { id });

export const touchProjectOpened = (id: number) =>
  invoke<void>("touch_project_opened", { id });

export const tasksList = (projectId: number) => invoke<TaskDto[]>("tasks_list", { projectId });

export const taskCreate = (projectId: number, title: string, status?: string) =>
  invoke<TaskDto>("task_create", { projectId, title, status });

export const taskUpdate = (id: number, patch: TaskPatch) =>
  invoke<TaskDto>("task_update", { id, patch });

export const taskMove = (id: number, status: string, position: number) =>
  invoke<TaskDto>("task_move", { id, status, position });

export const taskDelete = (id: number) => invoke<void>("task_delete", { id });

export const fsListDir = (projectId: number, relPath: string) =>
  invoke<FsEntry[]>("fs_list_dir", { projectId, relPath });

export const fsReadText = (projectId: number, relPath: string) =>
  invoke<string>("fs_read_text", { projectId, relPath });

export const fsWriteText = (projectId: number, relPath: string, content: string) =>
  invoke<FsEntry>("fs_write_text", { projectId, relPath, content });

export const fsCreateFolder = (projectId: number, parentRel: string, name: string) =>
  invoke<FsEntry>("fs_create_folder", { projectId, parentRel, name });

export const fsCreateFile = (projectId: number, parentRel: string, name: string) =>
  invoke<FsEntry>("fs_create_file", { projectId, parentRel, name });

export const fsRename = (projectId: number, relPath: string, newName: string) =>
  invoke<FsEntry>("fs_rename", { projectId, relPath, newName });

export const fsDuplicate = (projectId: number, relPath: string) =>
  invoke<FsEntry>("fs_duplicate", { projectId, relPath });

export const fsDelete = (projectId: number, relPath: string) =>
  invoke<void>("fs_delete", { projectId, relPath });

export const fsMove = (projectId: number, fromRel: string, toDirRel: string) =>
  invoke<FsEntry>("fs_move", { projectId, fromRel, toDirRel });

export const fsImport = (projectId: number, destRel: string, sourcePaths: string[]) =>
  invoke<FsEntry[]>("fs_import", { projectId, destRel, sourcePaths });

export const fsOpenExternal = (projectId: number, relPath: string) =>
  invoke<void>("fs_open_external", { projectId, relPath });

export const searchProject = (projectId: number, query: string, limit?: number) =>
  invoke<SearchHit[]>("search_project", { projectId, query, limit });

export const fsReadBinaryDataUrl = (projectId: number, relPath: string, maxBytes: number) =>
  invoke<string>("fs_read_binary_data_url", { projectId, relPath, maxBytes });

// Chat attachments (SPEC 6.1): attach_files copies sources into the project's
// or the harness inbox; exactly one of projectId / sessionId selects the
// target. The data-url/open commands resolve project-relative and absolute
// savedPaths alike (absolute also covers harness attachments and pending
// source paths before saving).
export const attachFiles = (
  projectId: number | null,
  sessionId: number | null,
  paths: string[],
) => invoke<AttachmentDto[]>("attach_files", { projectId, sessionId, paths });

export const attachmentDataUrl = (savedPath: string, projectId?: number, maxBytes?: number) =>
  invoke<string>("attachment_data_url", {
    projectId: projectId ?? null,
    savedPath,
    maxBytes: maxBytes ?? null,
  });

export const attachmentOpen = (savedPath: string, projectId?: number) =>
  invoke<void>("attachment_open", { projectId: projectId ?? null, savedPath });

export const reindexAll = () => invoke<ProjectIndexStats[]>("reindex_all");

export const embedStatus = () => invoke<EmbedStatus>("embed_status");

export const listModels = () => invoke<ModelInfo[]>("list_models");

export const testConnection = () => invoke<ConnectionTest>("test_connection");

// Vision support (SPEC 6.1): get_vision_status is cached/override-only,
// detect_vision probes the endpoint and caches the result in llm.json.
export const getVisionStatus = () => invoke<VisionStatus>("get_vision_status");

export const detectVision = () => invoke<VisionStatus>("detect_vision");

export const sendProjectMessage = (
  projectId: number,
  text: string,
  attachments?: MessageAttachmentDto[],
) =>
  invoke<number>("send_project_message", {
    projectId,
    text,
    attachments: attachments && attachments.length ? attachments : null,
  });

export const resolveConfirmation = (id: number, decision: "allow" | "deny" | "allowAlways") =>
  invoke<void>("resolve_confirmation", { id, decision });

export const projectChatHistory = (projectId: number) =>
  invoke<MessageDto[]>("project_chat_history", { projectId });

export const cancelRun = (projectId: number) => invoke<boolean>("cancel_run", { projectId });

// Harness sessions (SPEC 5.1): independent chats sharing only global memory.
export const harnessCreateSession = (title?: string) =>
  invoke<SessionDto>("harness_create_session", { title: title ?? null });

export const harnessListSessions = () => invoke<SessionDto[]>("harness_list_sessions");

export const harnessRenameSession = (id: number, title: string) =>
  invoke<SessionDto>("harness_rename_session", { id, title });

export const harnessDeleteSession = (id: number) => invoke<void>("harness_delete_session", { id });

export const harnessSessionHistory = (id: number) =>
  invoke<MessageDto[]>("harness_session_history", { id });

export const sendSessionMessage = (
  sessionId: number,
  text: string,
  attachments?: MessageAttachmentDto[],
) =>
  invoke<number>("send_session_message", {
    sessionId,
    text,
    attachments: attachments && attachments.length ? attachments : null,
  });

export const cancelSessionRun = (sessionId: number) =>
  invoke<boolean>("cancel_session_run", { sessionId });

// Scheduler (SPEC 5.3): run_at / cron / notify over the schedules table.
export const scheduleList = () => invoke<ScheduleDto[]>("schedule_list");

export const scheduleCreate = (
  kind: string,
  spec: string,
  prompt: string | null,
  timezone: string | null,
  sessionId: number | null,
) => invoke<ScheduleDto>("schedule_create", { kind, spec, prompt, timezone, sessionId });

export const schedulePause = (id: number) => invoke<ScheduleDto>("schedule_pause", { id });

export const scheduleResume = (id: number) => invoke<ScheduleDto>("schedule_resume", { id });

export const scheduleDelete = (id: number) => invoke<void>("schedule_delete", { id });

// Global memory (SPEC 6.3): CRUD for the settings UI; mutations emit
// "memory-changed" so open lists refresh.
export const memoryList = () => invoke<MemoryDto[]>("memory_list");

export const memoryAdd = (content: string, projectId?: number, tags?: string[]) =>
  invoke<MemoryDto>("memory_add", {
    content,
    projectId: projectId ?? null,
    tags: tags && tags.length ? tags : null,
  });

export const memoryUpdate = (id: number, content: string, tags?: string[]) =>
  invoke<MemoryDto>("memory_update", {
    id,
    content,
    tags: tags ? tags : null,
  });

export const memoryDelete = (id: number) => invoke<void>("memory_delete", { id });

export const memorySearch = (query: string, limit?: number) =>
  invoke<MemoryDto[]>("memory_search", { query, limit: limit ?? null });

// Skills management (SPEC 6.2): list/enable/import/install for the settings UI.
export const skillsList = () => invoke<SkillInfo[]>("skills_list");

export const skillsSetEnabled = (name: string, enabled: boolean) =>
  invoke<void>("skills_set_enabled", { name, enabled });

export const skillsImport = (source: string, target: string, overwrite?: boolean) =>
  invoke<ImportResult[]>("skills_import", { source, target, overwrite: overwrite ?? null });

export const skillsInstallBuiltin = () => invoke<string[]>("skills_install_builtin");

// skills_create: name is a slug (lowercase letters, digits, hyphens),
// description one line; kind defaults to "instruction" on the backend.
export const skillsCreate = (
  name: string,
  description: string,
  kind?: string,
  body?: string,
  overwrite?: boolean,
) =>
  invoke<SkillInfo>("skills_create", {
    name,
    description,
    kind: kind ?? null,
    body: body ?? null,
    overwrite: overwrite ?? null,
  });

// MCP (SPEC 6.2): per-server status and reload; enable/add edit mcp.json via
// read_config("mcp")/write_config("mcp") then reload.
export const mcpStatus = () => invoke<McpServerStatus[]>("mcp_status");

export const mcpReload = () => invoke<McpServerStatus[]>("mcp_reload");

// Backend events. Each returns an unlisten function.
export const onFsChanged = (handler: (payload: FsChanged) => void) =>
  listen<FsChanged>("fs-changed", (e) => handler(e.payload));

export const onTasksChanged = (handler: (payload: TasksChanged) => void) =>
  listen<TasksChanged>("tasks-changed", (e) => handler(e.payload));

export const onConfigChanged = (handler: (payload: ConfigChanged) => void) =>
  listen<ConfigChanged>("config-changed", (e) => handler(e.payload));

export const onIndexChanged = (handler: (payload: IndexChanged) => void) =>
  listen<IndexChanged>("index-changed", (e) => handler(e.payload));

export const onAgentEvent = (handler: (payload: AgentEventPayload) => void) =>
  listen<AgentEventPayload>("agent-event", (e) => handler(e.payload));

export const onAgentConfirmation = (handler: (payload: ConfirmationRequestPayload) => void) =>
  listen<ConfirmationRequestPayload>("agent-confirmation", (e) => handler(e.payload));

export const onSchedulesChanged = (handler: () => void) =>
  listen<unknown>("schedules-changed", () => handler());

// Emitted by the scheduler after a fired tick: the session list may have a
// new session (a schedule whose session was deleted creates one on fire) and
// the fired session's lastActiveAt changed.
export const onSessionsChanged = (handler: () => void) =>
  listen<unknown>("sessions-changed", () => handler());

export const onSkillsChanged = (handler: () => void) =>
  listen<unknown>("skills-changed", () => handler());

export const onMemoryChanged = (handler: (payload: MemoryChanged) => void) =>
  listen<MemoryChanged>("memory-changed", (e) => handler(e.payload));
