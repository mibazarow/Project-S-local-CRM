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

export interface TaskDto {
  id: number;
  projectId: number;
  title: string;
  status: string;
  comment: string | null;
  dueDate: string | null;
  remindAt: string | null;
  tags: string[];
  position: number;
  createdAt: string;
  closedAt: string | null;
  overdue: boolean;
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

// Distinguishes an explicit null (clear) from an absent field (keep).
export interface TaskPatch {
  title?: string;
  comment?: string | null;
  dueDate?: string | null;
  remindAt?: string | null;
  tags?: string[];
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
}

// ops::BackupInfo — result of create_backup.
export interface BackupInfo {
  path: string;
  entries: number;
  bytes: number;
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
  toolCalls: ToolCallDto[];
  toolCallId: string | null;
  name: string | null;
  createdAt: string;
  position: number;
  attachments: MessageAttachmentDto[];
}

// agent::loop AgentEvent (serde tag = "type", rename_all = "camelCase").
export type AgentEvent =
  | { type: "textDelta"; text: string }
  | { type: "toolCallStarted"; name: string; args: unknown }
  | { type: "toolCallFinished"; name: string; ok: boolean; envelope: unknown }
  | { type: "confirmationRequested"; tool: string; args: unknown; dangerReason: string | null }
  | { type: "done"; answer: string; reason: string }
  | { type: "error"; code: string; message: string };

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

// harness::SessionDto
export interface SessionDto {
  id: number;
  title: string | null;
  createdAt: string;
  updatedAt: string;
  lastActiveAt: string;
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
}

// journal::JournalEntry — one parsed line from Settings/logs/runs-*.jsonl.
export interface JournalEntryDto {
  ts: string;
  kind: string;
  chatId: number;
  projectId: number | null;
  sessionId: number | null;
  step: number;
  tool: string;
  ok: boolean;
  code: string | null;
  ms: number;
  args: string;
  result: string;
}

// journal::JournalListResult — result of journal_list: filtered entries plus
// the day/tool options for the "Журнал" filter bar.
export interface JournalListResult {
  entries: JournalEntryDto[];
  days: string[];
  tools: string[];
}
