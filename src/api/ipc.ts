import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AgentCompactionEvent,
  AgentEventPayload,
  AgentFileEvent,
  AttachmentDto,
  BackupInfo,
  BackupStatus,
  ChatBranchResult,
  ChecklistItem,
  ConfigChanged,
  ConfigKind,
  ConnectionTest,
  EmbedStatus,
  ExportSummary,
  FsChanged,
  FsEntry,
  GlobalHit,
  IndexChanged,
  ImportResult,
  ImportSummary,
  LlmProviderDto,
  McpServerStatus,
  MessageAttachmentDto,
  MessageDto,
  ModelInfo,
   NotesChanged,
   OnboardingStatus,
  OpenWithApp,
  OpenWithResult,
  ConfirmationRequestPayload,
   ProjectDto,
   ProjectIndexStats,
   QuickCaptureResult,
  ReindexDone,
  ReindexProgress,
  ReindexStatus,
    ScheduleDto,
  ScheduleOpenEvent,
  SessionDto,
  SkillInfo,
  SkillProposalEvent,
  TaskDto,
  TaskPatch,
  TasksChanged,
  TodoDto,
   TodosChanged,
   TrayAction,
   TrashChanged,
  TrashDto,
  TmpStats,
  VersionDto,
  VersionRestoreDto,
  VersionsChanged,
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

export const backupStatus = () => invoke<BackupStatus>("backup_status");

// Import/export (SPEC import/export slice). target selects the landing spot:
// "project" (needs projectId) or "notes". destRel is an optional existing
// subfolder inside the target; "" / null means the target root. Exports take
// an optional explicit destination .zip; null uses the backend default under
// Settings/backups/exports/.
export const importFolder = (
  target: "project" | "notes",
  projectId: number | null,
  sourcePath: string,
  destRel?: string,
) =>
  invoke<ImportSummary>("import_folder", {
    target,
    projectId,
    sourcePath,
    destRel: destRel ?? null,
  });

export const importArchive = (
  target: "project" | "notes",
  projectId: number | null,
  sourcePath: string,
  destRel?: string,
) =>
  invoke<ImportSummary>("import_archive", {
    target,
    projectId,
    sourcePath,
    destRel: destRel ?? null,
  });

export const importObsidian = (sourcePath: string, destRel?: string) =>
  invoke<ImportSummary>("import_obsidian", { sourcePath, destRel: destRel ?? null });

export const exportProject = (projectId: number, destPath?: string) =>
  invoke<ExportSummary>("export_project", { projectId, destPath: destPath ?? null });

export const exportNotes = (destPath?: string) =>
  invoke<ExportSummary>("export_notes", { destPath: destPath ?? null });

// Trash and version history (SPEC trash/versions slice). version_list/restore
// are keyed by (scope, absolute path); restore returns the stored content and
// the caller writes it back through the scope's own write command.
export const trashList = () => invoke<TrashDto[]>("trash_list");

export const trashRestore = (id: number) => invoke<TrashDto>("trash_restore", { id });

export const trashPurge = (id: number) => invoke<void>("trash_purge", { id });

export const versionList = (scope: string, path: string) =>
  invoke<VersionDto[]>("version_list", { scope, path });

export const versionRestore = (id: number) => invoke<VersionRestoreDto>("version_restore", { id });

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

export const tasksList = (projectId: number) => invoke<TaskDto[]>("tasks_list", { projectId });

export const taskCreate = (
  projectId: number,
  title: string,
  status?: string,
  dueDate?: string | null,
  dueTime?: string | null,
  isPriority?: boolean,
  checklist?: ChecklistItem[],
) =>
  invoke<TaskDto>("task_create", {
    projectId,
    title,
    status: status ?? null,
    dueDate: dueDate ?? null,
    dueTime: dueTime ?? null,
    isPriority: isPriority ?? null,
    checklist: checklist ?? null,
  });

export const taskUpdate = (id: number, patch: TaskPatch) =>
  invoke<TaskDto>("task_update", { id, patch });

export const taskMove = (id: number, status: string, position: number) =>
  invoke<TaskDto>("task_move", { id, status, position });

export const taskDelete = (id: number) => invoke<void>("task_delete", { id });

// Entity links (links slice): context actions record note ↔ file/chat and
// task ↔ chat after creating the entity; the agent loop records tool-call
// links itself. Deduped backend-side, so a repeated call is a no-op.
export const linkCreate = (
  fromKind: string,
  fromId: string,
  toKind: string,
  toId: string,
) => invoke<void>("link_create", { fromKind, fromId, toKind, toId });

// Per-conversation plans (todowrite/todoread tools): read-only panel data.
export const todosList = (scopeKind: "project" | "session", scopeId: number) =>
  invoke<TodoDto[]>("todos_list", { scopeKind, scopeId });

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

// File context menu (fs_copy_as merged the former fs_duplicate «Дублировать»).
export const fsCopyAs = (projectId: number, relPath: string) =>
  invoke<FsEntry>("fs_copy_as", { projectId, relPath });

export const fsAbsolutePath = (projectId: number, relPath: string) =>
  invoke<string>("fs_absolute_path", { projectId, relPath });

export const fsRevealInFinder = (projectId: number, relPath: string) =>
  invoke<void>("fs_reveal_in_finder", { projectId, relPath });

export const fsExtractArchive = (projectId: number, relPath: string) =>
  invoke<FsEntry>("fs_extract_archive", { projectId, relPath });

export const fsDelete = (projectId: number, relPath: string) =>
  invoke<void>("fs_delete", { projectId, relPath });

export const fsMove = (projectId: number, fromRel: string, toDirRel: string) =>
  invoke<FsEntry>("fs_move", { projectId, fromRel, toDirRel });

export const fsImport = (projectId: number, destRel: string, sourcePaths: string[]) =>
  invoke<FsEntry[]>("fs_import", { projectId, destRel, sourcePaths });

export const fsOpenExternal = (projectId: number, relPath: string) =>
  invoke<void>("fs_open_external", { projectId, relPath });

// "Open with" chooser: non-stock apps for an extension, the remembered
// per-extension choice, and opening a file with a chosen app.
export const listOpenWithApps = (ext: string) =>
  invoke<OpenWithApp[]>("list_open_with_apps", { ext });

export const getOpenWith = (ext: string) => invoke<OpenWithApp | null>("get_open_with", { ext });

export const rememberOpenWith = (ext: string, appPath: string) =>
  invoke<void>("remember_open_with", { ext, appPath });

export const openWith = (
  scope: "project" | "notes" | "harness",
  path: string,
  projectId?: number,
  appPath?: string,
) =>
  invoke<OpenWithResult>("open_with", {
    scope,
    projectId: projectId ?? null,
    path,
    appPath: appPath ?? null,
  });

export const fsReadBinaryDataUrl = (projectId: number, relPath: string, maxBytes: number) =>
  invoke<string>("fs_read_binary_data_url", { projectId, relPath, maxBytes });

// Notes vault (top-level Notes/ folder): markdown notes with folders, all
// paths vault-relative. Mutations emit "notes-changed"; agent tool writes
// arrive later as "index-changed" with projectId = -1 (NOTES_SCOPE_ID).
export const notesEnsure = () => invoke<string>("notes_ensure");

export const notesTree = () => invoke<FsEntry[]>("notes_tree");

export const notesList = (rel?: string) => invoke<FsEntry[]>("notes_list", { rel: rel ?? null });

export const notesRead = (rel: string) => invoke<string>("notes_read", { rel });

// Binary preview read for non-md vault files (images/PDF as base64 data
// URLs); text files keep notesRead. Same caps as fsReadBinaryDataUrl.
export const notesReadBinaryDataUrl = (rel: string, maxBytes: number) =>
  invoke<string>("notes_read_binary_data_url", { rel, maxBytes });

export const notesWrite = (rel: string, content: string) =>
  invoke<FsEntry>("notes_write", { rel, content });

export const notesCreate = (rel: string, content?: string) =>
  invoke<FsEntry>("notes_create", { rel, content: content ?? null });

export const notesRename = (rel: string, newName: string) =>
  invoke<FsEntry>("notes_rename", { rel, newName });

export const notesMove = (fromRel: string, toDirRel: string) =>
  invoke<FsEntry>("notes_move", { fromRel, toDirRel });

// Finder drag-and-drop copy into the vault (SPEC 4.2): sources are copied,
// never moved; destRel "" is the vault root. Emits "notes-changed" per item.
export const notesImport = (destRel: string, sourcePaths: string[]) =>
  invoke<FsEntry[]>("notes_import", { destRel, sourcePaths });

export const notesDelete = (rel: string) => invoke<void>("notes_delete", { rel });

// Quick capture (SPEC quick capture slice): one entry from the capture modal
// (global shortcut / tray menu). target "note" appends to the daily
// Notes/Входящие note; target "task" creates a task, projectId null falls
// back to the first project.
export const quickCapture = (
  text: string,
  target: "note" | "task",
  projectId?: number | null,
) => invoke<QuickCaptureResult>("quick_capture", { text, target, projectId: projectId ?? null });

// Reusable templates (Settings/config/templates.json, seeded on first use):
// the create command instantiates one template as a project. Note templates
// were removed (Q-15); project/task/document templates stay.
export const templateCreateProject = (templateName: string, projectName?: string) =>
  invoke<ProjectDto>("template_create_project", {
    templateName,
    projectName: projectName ?? null,
  });

// Notes chat: one continuous chat per working dir; events route by
// "notes: true" on agent-event / agent-confirmation.
export const ensureNotesChat = () => invoke<number>("ensure_notes_chat");

export const notesChatHistory = () => invoke<MessageDto[]>("notes_chat_history");

export const sendNotesMessage = (text: string, attachments?: MessageAttachmentDto[]) =>
  invoke<number>("send_notes_message", { text, attachments: attachments ?? null });

export const cancelNotesRun = () => invoke<boolean>("cancel_notes_run");

// Chat attachments (SPEC 6.1): attach_files copies sources into the project's,
// the harness or the Notes inbox; exactly one of projectId / sessionId / notes
// selects the target. The data-url/open commands resolve project-relative and
// absolute savedPaths alike (absolute also covers harness and notes
// attachments and pending source paths before saving).
export const attachFiles = (
  projectId: number | null,
  sessionId: number | null,
  paths: string[],
  notes?: boolean,
) =>
  invoke<AttachmentDto[]>("attach_files", {
    projectId,
    sessionId,
    notes: notes ?? null,
    paths,
  });

export const attachmentDataUrl = (savedPath: string, projectId?: number, maxBytes?: number) =>
  invoke<string>("attachment_data_url", {
    projectId: projectId ?? null,
    savedPath,
    maxBytes: maxBytes ?? null,
  });

export const attachmentOpen = (savedPath: string, projectId?: number) =>
  invoke<void>("attachment_open", { projectId: projectId ?? null, savedPath });

// Agent-sent chat files (SPEC 6.1): the savedPath of a send_file attachment is
// resolved by the chat's kind, so reads/opens go through the chat/message/index
// instead of a savedPath + projectId pair.
export const chatAttachmentDataUrl = (
  chatId: number,
  messageId: number,
  index: number,
  maxBytes?: number,
) =>
  invoke<string>("chat_attachment_data_url", {
    chatId,
    messageId,
    index,
    maxBytes: maxBytes ?? null,
  });

export const chatAttachmentOpen = (chatId: number, messageId: number, index: number) =>
  invoke<void>("chat_attachment_open", { chatId, messageId, index });

export const reindexAll = () => invoke<ProjectIndexStats[]>("reindex_all");

// Hard rebuild: clears the whole file index (all projects + notes) in one
// transaction and reindexes everything from scratch.
export const reindexHard = () => invoke<ProjectIndexStats[]>("reindex_hard");

// Style profile (SPEC 6.6): reset the global style-of-communication profile the
// agents mirror from the user's messages.
export const styleReset = () => invoke<void>("style_reset");

// Search reindex («Переиндексация поиска»): manual start (no-op while a pass
// runs) and read-only status; the same pass also fires automatically every
// 5 minutes. Progress arrives as "reindex-progress" / "reindex-done".
export const reindexStart = () => invoke<ReindexStatus>("reindex_start");

export const reindexStatus = () => invoke<ReindexStatus>("reindex_status");

export const embedStatus = () => invoke<EmbedStatus>("embed_status");

export const listModels = () => invoke<ModelInfo[]>("list_models");

export const testConnection = () => invoke<ConnectionTest>("test_connection");

// Vision support (SPEC 6.1): get_vision_status is cached/override-only,
// detect_vision probes the endpoint and caches the result in llm.json.
export const getVisionStatus = () => invoke<VisionStatus>("get_vision_status");

export const detectVision = () => invoke<VisionStatus>("detect_vision");

// Provider profiles (SPEC 3.4 Q-24): each mutation returns the fresh list so
// the UI refreshes without a second read. Switching the active provider is
// settings-only; the backend mirrors it into the legacy llm.json fields.
export const llmProvidersList = () => invoke<LlmProviderDto[]>("llm_providers_list");

export const llmProviderAdd = (name: string, baseUrl: string, apiKey: string) =>
  invoke<LlmProviderDto[]>("llm_provider_add", { name, baseUrl, apiKey });

export const llmProviderUpdate = (
  id: string,
  name: string,
  baseUrl: string,
  apiKey: string,
) => invoke<LlmProviderDto[]>("llm_provider_update", { id, name, baseUrl, apiKey });

export const llmProviderDelete = (id: string) =>
  invoke<LlmProviderDto[]>("llm_provider_delete", { id });

export const llmProviderActivate = (id: string) =>
  invoke<LlmProviderDto[]>("llm_provider_activate", { id });

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
// `greeting` seeds the chat with one persisted static assistant message (no
// LLM call) — used by the schedules panel «+» to open a schedule-creation chat.
export const harnessCreateSession = (title?: string, greeting?: string) =>
  invoke<SessionDto>("harness_create_session", {
    title: title ?? null,
    greeting: greeting ?? null,
  });

export const harnessListSessions = () => invoke<SessionDto[]>("harness_list_sessions");

export const harnessRenameSession = (id: number, title: string) =>
  invoke<SessionDto>("harness_rename_session", { id, title });

export const harnessDeleteSession = (id: number) => invoke<void>("harness_delete_session", { id });

// Pin/unpin a Harness chat; returns the fresh session list (pinned first).
export const harnessPinSession = (id: number, pinned: boolean) =>
  invoke<SessionDto[]>("harness_pin_session", { id, pinned });

export const harnessSessionHistory = (id: number) =>
  invoke<MessageDto[]>("harness_session_history", { id });

// Message-level chat UX: pin, truncate (regenerate), branch (harness only),
// in-chat LIKE search.
export const chatPin = (chatId: number, messageId: number, pinned: boolean) =>
  invoke<void>("chat_pin", { chatId, messageId, pinned });

export const chatTruncateAfter = (chatId: number, messageId: number) =>
  invoke<number>("chat_truncate_after", { chatId, messageId });

export const chatBranch = (chatId: number, messageId: number) =>
  invoke<ChatBranchResult>("chat_branch", { chatId, messageId });

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

// Global cross-entity search (⌘K palette): projects, files, tasks, notes and
// harness chats in one merged, ranked list.
export const globalSearch = (query: string, limit?: number) =>
  invoke<GlobalHit[]>("global_search", { query, limit: limit ?? null });

export const indexChatMessages = () => invoke<number>("index_chat_messages");

// Scheduler (SPEC 5.3): run_at / cron / notify over the schedules table.
// Creation goes through the agent's schedule tools from a chat, not an IPC form.
export const scheduleList = () => invoke<ScheduleDto[]>("schedule_list");

export const schedulePause = (id: number) => invoke<ScheduleDto>("schedule_pause", { id });

export const scheduleResume = (id: number) => invoke<ScheduleDto>("schedule_resume", { id });

export const scheduleDelete = (id: number) => invoke<void>("schedule_delete", { id });

// Click-through: returns the session whose chat the UI opens (the schedule's
// own, or a fresh one titled after the schedule created by the backend).
export const scheduleOpen = (id: number) =>
  invoke<{ sessionId: number }>("schedule_open", { id });

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

// MCP (SPEC 6.2): per-server status and reload; add/edit/delete go through
// read_config("mcp")/write_config("mcp") then reload; the enabled toggle is
// the dedicated command (persists mcp.json, restarts the server, emits
// config-changed).
export const mcpStatus = () => invoke<McpServerStatus[]>("mcp_status");

export const mcpReload = () => invoke<McpServerStatus[]>("mcp_reload");

export const mcpSetEnabled = (name: string, enabled: boolean) =>
  invoke<McpServerStatus[]>("mcp_set_enabled", { name, enabled });

// Backend events. Each returns an unlisten function.
export const onFsChanged = (handler: (payload: FsChanged) => void) =>
  listen<FsChanged>("fs-changed", (e) => handler(e.payload));

export const onTasksChanged = (handler: (payload: TasksChanged) => void) =>
  listen<TasksChanged>("tasks-changed", (e) => handler(e.payload));

export const onConfigChanged = (handler: (payload: ConfigChanged) => void) =>
  listen<ConfigChanged>("config-changed", (e) => handler(e.payload));

export const onIndexChanged = (handler: (payload: IndexChanged) => void) =>
  listen<IndexChanged>("index-changed", (e) => handler(e.payload));

// Emitted by the reindex pass after each unit (project/notes/tasks/chats/memory).
export const onReindexProgress = (handler: (payload: ReindexProgress) => void) =>
  listen<ReindexProgress>("reindex-progress", (e) => handler(e.payload));

// Emitted once when a reindex pass finishes.
export const onReindexDone = (handler: (payload: ReindexDone) => void) =>
  listen<ReindexDone>("reindex-done", (e) => handler(e.payload));

export const onAgentEvent = (handler: (payload: AgentEventPayload) => void) =>
  listen<AgentEventPayload>("agent-event", (e) => handler(e.payload));

export const onAgentConfirmation = (handler: (payload: ConfirmationRequestPayload) => void) =>
  listen<ConfirmationRequestPayload>("agent-confirmation", (e) => handler(e.payload));

// Emitted by the send_file tool after the agent attached a file to a chat.
export const onAgentFile = (handler: (payload: AgentFileEvent) => void) =>
  listen<AgentFileEvent>("agent-file", (e) => handler(e.payload));

// Emitted when a run's context crossed the compaction threshold and a new
// checkpoint summary was written (the summary rides the event only).
export const onAgentCompaction = (handler: (payload: AgentCompactionEvent) => void) =>
  listen<AgentCompactionEvent>("agent-context-compacted", (e) => handler(e.payload));

// Emitted by the repeat detector after a run completed the same complex
// task for the second time (SPEC 6.2 Q20 skill proposal).
export const onSkillProposal = (handler: (payload: SkillProposalEvent) => void) =>
  listen<SkillProposalEvent>("agent-skill-proposal", (e) => handler(e.payload));

export const onSchedulesChanged = (handler: () => void) =>
  listen<unknown>("schedules-changed", () => handler());

// Emitted when the UI must switch to a session's chat: the notification click
// handler in lib.rs (a schedule-run completion) and the schedule_create tool
// (a freshly created schedule chat, Q.md item 4).
export const onScheduleOpen = (handler: (payload: ScheduleOpenEvent) => void) =>
  listen<ScheduleOpenEvent>("schedule-open", (e) => handler(e.payload));

// Emitted when the session list may have changed: the scheduler after a
// fired tick (a fire touches lastActiveAt and may create a fresh session),
// the session auto-title after the first message, and the rename_chat tool.
export const onSessionsChanged = (handler: () => void) =>
  listen<unknown>("sessions-changed", () => handler());

export const onSkillsChanged = (handler: () => void) =>
  listen<unknown>("skills-changed", () => handler());

export const onProjectsChanged = (handler: () => void) =>
  listen<unknown>("projects-changed", () => handler());

// Emitted by the todowrite tool after the plan of a scope changed.
export const onTodosChanged = (handler: (payload: TodosChanged) => void) =>
  listen<TodosChanged>("todos-changed", (e) => handler(e.payload));

// Emitted by the notes_* mutation commands after a note or folder changed.
export const onNotesChanged = (handler: (payload: NotesChanged) => void) =>
  listen<NotesChanged>("notes-changed", (e) => handler(e.payload));

// Emitted after any trash mutation (delete/restore/purge); refresh trash lists.
export const onTrashChanged = (handler: (payload: TrashChanged) => void) =>
  listen<TrashChanged>("trash-changed", (e) => handler(e.payload));

// Emitted after a version snapshot is taken for (scope, absolute path); the
// version-history panel reloads when its own key matches.
export const onVersionsChanged = (handler: (payload: VersionsChanged) => void) =>
  listen<VersionsChanged>("versions-changed", (e) => handler(e.payload));

// Emitted by the tray menu after showing the window: routes «Быстрая
// заметка» / «Новая задача» to the capture modal.
export const onTrayAction = (handler: (payload: TrayAction) => void) =>
  listen<TrayAction>("tray-action", (e) => handler(e.payload));

// Emitted by the global CmdOrCtrl+Shift+Space shortcut after showing the
// window: open the capture modal.
export const onQuickCapture = (handler: () => void) =>
  listen<unknown>("quick-capture", () => handler());
