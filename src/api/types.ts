// DTOs mirroring the Rust structs (serde rename_all = "camelCase").

export interface OnboardingStatus {
  configured: boolean;
  workingDir: string | null;
}

export interface WorkingDirInfo {
  workingDir: string;
  configDir: string;
}

export interface ProjectDto {
  id: number;
  name: string;
  path: string;
  createdAt: string;
  openedAt: string | null;
  position: number;
  fileCount: number;
  taskCount: number;
  overdueCount: number;
  // True when the project has at least one task in "В работе" (status
  // progress); drives the Play indicator on the project row (SPEC 4.1).
  hasActiveTasks: boolean;
  // Per-project kanban column labels, always exactly 3 (projects.rs
  // task_columns); renaming one column replaces the whole triple.
  taskColumns: string[];
  // Per-project file view mode (SPEC 4.2): remembered list/grid, stored in
  // the project DB; unknown backend values normalize to "list".
  filesView: "list" | "grid";
}

export interface FsEntry {
  name: string;
  relPath: string;
  kind: "dir" | "file";
  ext: string;
  size: number;
  modified: number;
  isMarkdown: boolean;
  isText: boolean;
  isImage: boolean;
}

// "Open with" chooser: a non-stock macOS app offered for a file extension.
export interface OpenWithApp {
  name: string;
  path: string;
  recommended: boolean;
}

export interface OpenWithResult {
  opened: boolean;
  app?: string | null;
}

// tasks::ChecklistItem — one checklist row of a task.
export interface ChecklistItem {
  text: string;
  done: boolean;
}

export interface TaskDto {
  id: number;
  projectId: number;
  title: string;
  status: string;
  comment: string | null;
  dueDate: string | null;
  dueTime: string;
  remindAt: string | null;
  position: number;
  createdAt: string;
  closedAt: string | null;
  overdue: boolean;
  isPriority: boolean;
  checklist: ChecklistItem[];
  recurrence: string;
  // Wall-clock timer (SPEC 4.3): accumulated spent ms; startedAt is the
  // active start in epoch ms, non-null exactly while status is "progress".
  spentMs: number;
  startedAt: number | null;
}

// todos::TodoDto — one plan item of a scope (project chat or harness session).
export interface TodoDto {
  id: number;
  text: string;
  done: boolean;
  position: number;
}

// Payload of the "todos-changed" event emitted by the todowrite tool.
export interface TodosChanged {
  scopeKind: "project" | "session";
  scopeId: number;
}

// objectives::ObjectiveStepDto — one step of a goal.
export interface ObjectiveStepDto {
  id: number;
  text: string;
  done: boolean;
  position: number;
}

// objectives::ObjectiveDto — a long-lived goal with its steps and progress.
export interface ObjectiveDto {
  id: number;
  title: string;
  description: string | null;
  status: "active" | "paused" | "done" | "error";
  createdAt: string;
  updatedAt: string;
  steps: ObjectiveStepDto[];
  doneSteps: number;
  totalSteps: number;
}

// Payload of the "objectives-changed" event emitted after any objective mutation.
export interface ObjectivesChanged {
  id: number | null;
}

export interface SearchHit {  relPath: string;
  name: string;
  kind: string;
  score: number;
  snippet: string;
  matchTypes: string[];
  modified: number;
  taskId?: number | null;
  status?: string | null;
  overdue?: boolean | null;
}

// search::GlobalHit — one hit of the cross-entity ⌘K palette search.
// kind selects the navigation target; the id fields tag it (project/task/
// session id, files_fts rowid for file/note hits). Snippets carry FTS5
// markers ("<<match>>").
export interface GlobalHit {
  kind: "project" | "file" | "task" | "note" | "chat";
  id: number;
  title: string;
  subtitle: string;
  snippet: string;
  score: number;
  projectId?: number | null;
  sessionId?: number | null;
  taskId?: number | null;
  relPath?: string | null;
}

// Distinguishes an explicit null (clear) from an absent field (keep).
// dueTime is NOT NULL backend-side: null/absent keeps the current value.
export interface TaskPatch {
  title?: string;
  comment?: string | null;
  dueDate?: string | null;
  dueTime?: string;
  remindAt?: string | null;
  isPriority?: boolean;
  checklist?: ChecklistItem[];
  recurrence?: string;
}

export type ConfigKind = "llm" | "app" | "indexing" | "mcp";

export interface FsChanged {
  projectId: number;
  relPath: string;
}

export interface TasksChanged {
  projectId: number;
}

export interface ConfigChanged {
  which: ConfigKind;
}

// Shape of app.json (config.rs AppConfig). Agent identity is fixed
// ("Project-S", informal «ты»): old files may still carry agentName /
// userAddress keys, the app ignores them.
export interface AppConfig {
  autopilot: boolean;
  compactionThresholdPercent: number;
  sessionTtlDays: number;
  tmpRetentionDays: number;
  logRetentionDays: number;
  autostart: boolean;
  // Automatic backup interval in days (SPEC 3.6); 0 disables auto-backup.
  backupIntervalDays: number;
  // How many newest backups auto-backup retention keeps (SPEC 3.6).
  backupKeep: number;
}

// ops::BackupInfo — result of create_backup.
export interface BackupInfo {
  path: string;
  entries: number;
  bytes: number;
}

// importer::ImportSummary — result of import_folder / import_archive /
// import_obsidian. skipped counts folders the importer deliberately ignored
// (Obsidian internals, symlinks); dest is where the import landed.
export interface ImportSummary {
  importedFiles: number;
  skipped: number;
  dest: string;
}

// importer::ExportSummary — result of export_project / export_notes.
export interface ExportSummary {
  path: string;
  files: number;
}

// ops::BackupStatus — snapshot of the auto-backup state (backup_status).
// lastBackupAt is RFC3339 UTC; due means a backup is overdue right now.
export interface BackupStatus {
  lastBackupAt: string | null;
  intervalDays: number;
  keep: number;
  count: number;
  due: boolean;
}

// reliability::TrashDto — one row of trash_list. kind is "file" | "dir" |
// "task"; scope is "project" | "notes" | "harness". storedPath is empty for
// task snapshots (the whole item lives in `meta`).
export interface TrashDto {
  id: number;
  scope: string;
  kind: string;
  originalPath: string;
  storedPath: string;
  name: string;
  deletedAt: string;
  meta: string | null;
  // Filled only when a restored task is returned, so callers can refresh the
  // owning project's board.
  projectId?: number | null;
}

// reliability::VersionDto — one snapshot row of version_list (keyed by the
// absolute path of the file).
export interface VersionDto {
  id: number;
  scope: string;
  path: string;
  bytes: number;
  createdAt: string;
}

// reliability::VersionRestoreDto — result of version_restore; the caller
// writes `content` back through the scope's own write command.
export interface VersionRestoreDto {
  id: number;
  scope: string;
  path: string;
  content: string;
}

// lib.rs TrashChanged — payload of the "trash-changed" event.
export interface TrashChanged {
  id: number | null;
}

// lib.rs VersionsChanged — payload of the "versions-changed" event, keyed by
// the same (scope, absolute path) pair as version_list.
export interface VersionsChanged {
  scope: string;
  path: string;
}

// ops::TmpStats — result of clear_tmp.
export interface TmpStats {
  removedFiles: number;
  freedBytes: number;
}

// Shape of llm.json (config.rs LlmConfig).
export interface LlmConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
  contextWindow: number;
  temperature: number | null;
  maxTokens: number | null;
  // Manual vision override: null/absent = auto-detect, true/false = force.
  vision?: boolean | null;
  // Cached result of the last vision probe (written by detect_vision).
  visionDetected?: boolean | null;
  // Thinking mode level: "off" omits reasoning_effort, the rest send it as-is.
  thinkingLevel?: "off" | "low" | "medium" | "high";
  // Saved provider profiles (Q-24): any number, exactly one active. The
  // top-level baseUrl/apiKey mirror the active provider (legacy fallback).
  providers?: LlmProvider[];
  activeProvider?: string | null;
}

// A saved provider profile (config.rs LlmProvider). baseUrl/apiKey are the
// endpoint and key; name is the user label.
export interface LlmProvider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
}

// llm::LlmProviderDto — a provider row returned by the provider commands.
export interface LlmProviderDto extends LlmProvider {
  active: boolean;
}

// llm::VisionStatus — result of get_vision_status / detect_vision.
export interface VisionStatus {
  // null = unknown (never probed, or the probe was inconclusive).
  supported: boolean | null;
  // True when llm.json `vision` manually overrides detection.
  overridden: boolean;
  reason: string | null;
}

export interface ModelInfo {
  id: string;
}

// Result of test_connection (llm::ConnectionTest).
export interface ConnectionTest {
  ok: boolean;
  message: string;
  modelCount: number;
}

// Shape of indexing.json (config.rs IndexingConfig). The old `embedding`
// object is gone: semantic search is always on (in-app ORT +
// EmbeddingGemma 2, dim 768) and not configurable; old files that still
// carry that key load unchanged.
export interface IndexingConfig {
  maxFileSizeBytes: number;
  maxTextChars: number;
  excludeDirs: string[];
}

// Result of embed_status: "ort" when the model files are on disk, "noop"
// only as the internal fallback (FTS-only search).
export interface EmbedStatus {
  backend: string;
  dim: number;
  available: boolean;
  modelPresent: boolean;
  reason?: string | null;
}

export interface IndexStats {
  filesSeen: number;
  filesIndexed: number;
  filesSkipped: number;
  filesRemoved: number;
  errors: string[];
}

export interface ProjectIndexStats {
  projectId: number;
  stats: IndexStats;
}

export interface IndexChanged {
  projectId: number;
  stats: IndexStats;
}

// AppError is serialized by the backend as { code, message }.
export interface AppErrorPayload {
  code: string;
  message: string;
}

// chat::MessageDto
export interface ToolCallDto {
  id: string;
  name: string;
  arguments: string;
}

// attachments::AttachmentDto — one saved copy returned by attach_files.
// savedPath is project-relative for project chats, absolute for harness ones.
export interface AttachmentDto {
  name: string;
  savedPath: string;
  kind: "image" | "document" | "other";
  size: number;
  isImage: boolean;
  isText: boolean;
}

// attachments::AttachmentRef — send_* input and MessageDto.attachments rows.
export interface MessageAttachmentDto {
  name: string;
  savedPath: string;
  kind: "image" | "document" | "other";
  isImage: boolean;
}

export interface MessageDto {
  id: number;
  chatId: number;
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  // Thinking trace of an assistant turn (null when thinking is off).
  reasoning: string | null;
  toolCalls: ToolCallDto[];
  toolCallId: string | null;
  name: string | null;
  createdAt: string;
  position: number;
  attachments: MessageAttachmentDto[];
  // Pinned messages show in the compact bar above the transcript.
  pinned: boolean;
}

// chat::ChatBranchDto — result of chat_branch (harness chats only).
export interface ChatBranchResult {
  chatId: number;
  sessionId: number;
}

// chat::ChatSearchHitDto — one LIKE hit inside a chat.
export interface ChatSearchHit {
  messageId: number;
  role: string;
  snippet: string;
  createdAt: string;
}

// agent::loop AgentEvent (serde tag = "type", rename_all = "camelCase").
export type AgentEvent =
  | { type: "textDelta"; text: string }
  | { type: "reasoningDelta"; text: string }
  | { type: "toolCallStarted"; name: string; args: unknown }
  | { type: "toolCallFinished"; name: string; ok: boolean; envelope: unknown }
  | { type: "confirmationRequested"; tool: string; args: unknown; dangerReason: string | null }
  | { type: "done"; answer: string; reason: string; reasoning: string | null }
  | { type: "error"; code: string; message: string };

// agent::send_file — payload of the "agent-file" event emitted when the agent
// attaches a file to the chat with send_file. savedPath rules match
// MessageAttachmentDto (project-/Notes-vault-relative, absolute for harness).
export interface AgentFileEvent {
  chatId: number;
  messageId: number;
  name: string;
  path: string;
  kind: "image" | "document" | "other";
  isImage: boolean;
  caption: string;
}

// chat::run_agent_inner — payload of the "agent-context-compacted" event
// emitted when a run's build_context folds old turns into a new checkpoint.
// summary is the new checkpoint text shown as a collapsible status item; it
// is never persisted as a chat message.
export interface AgentCompactionEvent {
  chatId: number;
  upToPosition: number;
  summary: string;
}

// skills::proposals — payload of the "agent-skill-proposal" event emitted
// when the repeat detector posts its one-time skill proposal into a chat
// (SPEC 6.2 Q20). The text is already persisted as the assistant message
// (messageId); an open chat appends it live and a later history reload owns
// the same id, so it never renders twice.
export interface SkillProposalEvent {
  chatId: number;
  messageId: number;
  text: string;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
}

// chat::AgentEventPayload — project chats carry projectId, harness sessions
// carry sessionId, notes-chat runs carry notes: true (chatId only).
export interface AgentEventPayload {
  chatId: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  event: AgentEvent;
}

// chat::ConfirmationRequestPayload ("agent-confirmation" event), routed by
// the same run key: projectId for project chats, sessionId for sessions,
// notes: true for notes-chat runs.
export interface ConfirmationRequestPayload {
  id: number;
  projectId?: number;
  sessionId?: number;
  notes?: boolean;
  tool: string;
  args: unknown;
  dangerReason: string | null;
}

// lib.rs NotesChanged — payload of the "notes-changed" event emitted by the
// notes_* mutation commands.
export interface NotesChanged {
  relPath: string;
}

// lib.rs TrayAction — payload of the "tray-action" event emitted by the
// menu-bar menu after the window was shown; "quit" is handled natively.
export interface TrayAction {
  action: "open" | "quick-note" | "new-task";
}

// lib.rs QuickCaptureResult — result of the quick_capture command.
export interface QuickCaptureResult {
  kind: "note" | "task";
  path: string | null;
  taskId: number | null;
}

// harness::SessionDto
export interface SessionDto {
  id: number;
  title: string | null;
  createdAt: string;
  updatedAt: string;
  lastActiveAt: string;
}

// search::ChatHit — one harness session matched by search_chats; the snippet
// carries FTS5 markers ("<<match>>"), score is the merged RRF relevance.
export interface ChatHit {
  sessionId: number;
  title: string | null;
  lastActive: string;
  score: number;
  messageId: number;
  snippet: string;
}

// skills::SkillInfo — one row of skills_list. Invalid or builtin-shadowed
// rows carry `error` and have no toggle; the rest are normal skills.
export interface SkillInfo {
  name: string;
  description: string;
  kind: string;
  builtin: boolean;
  enabled: boolean;
  path: string;
  error: string | null;
}

// skills::ImportResult — per-skill outcome of skills_import.
export interface ImportResult {
  name: string;
  ok: boolean;
  error: string | null;
}

// mcp::McpServerStatus — one row of mcp_status / mcp_reload.
export interface McpServerStatus {
  name: string;
  enabled: boolean;
  // "stdio" | "http" | "sse" | "unknown" (broken config row).
  transport: string;
  status: "running" | "error" | "disabled";
  error: string | null;
  toolCount: number;
}

// memory::MemoryDto — one global memory record (SPEC 6.3).
export interface MemoryDto {
  id: number;
  content: string;
  projectId: number | null;
  tags: string[];
  source: string | null;
  createdAt: string;
  updatedAt: string;
}

// memory::MemoryChanged — payload of the "memory-changed" event.
export interface MemoryChanged {
  id: number | null;
}

// scheduler::ScheduleDto
export interface ScheduleDto {
  id: number;
  sessionId: number | null;
  kind: "run_at" | "cron" | "notify";
  spec: string;
  prompt: string | null;
  status: "active" | "paused" | "done" | "error";
  relatedTaskId: number | null;
  createdAt: string;
  lastRunAt: string | null;
  nextRunAt: string | null;
  timezone: string | null;
  error: string | null;
  title: string | null;
}

// search_reindex::ReindexStatus — result of reindex_start / reindex_status.
// nextRunAt/lastRunAt are epoch milliseconds; nextRunAt is the auto-reindex
// countdown target (0 until armed).
export interface ReindexStatus {
  running: boolean;
  done: number;
  total: number;
  phase: string;
  nextRunAt: number;
  lastRunAt: number | null;
}

// "reindex-progress" event payload: emitted after each unit of a pass.
export interface ReindexProgress {
  done: number;
  total: number;
  phase: string;
}

// "reindex-done" event payload: emitted once a pass finishes.
export interface ReindexDone {
  done: number;
  total: number;
}
