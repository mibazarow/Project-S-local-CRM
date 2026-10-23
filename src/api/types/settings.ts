// Settings/config DTOs: app/indexing configs, ops (backup/tmp), import/export
// summaries and the index/reindex stats.

export type ConfigKind = "llm" | "app" | "indexing" | "mcp";

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
  // Whole-UI scale as a percentage (Q.md item 4). Only the six shipped steps
  // (100/110/125/150/175/200) are valid; anything else normalizes to 100.
  uiScalePercent: number;
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

// ops::TmpStats — result of clear_tmp.
export interface TmpStats {
  removedFiles: number;
  freedBytes: number;
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
