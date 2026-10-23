// Settings IPC: config read/write, autostart, ops (tmp/backup), LLM models,
// vision, provider profiles and the config-changed event.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  BackupInfo,
  BackupStatus,
  ConfigChanged,
  ConfigKind,
  ConnectionTest,
  LlmProviderDto,
  ModelInfo,
  TmpStats,
  VisionStatus,
} from "../types";

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

export const onConfigChanged = (handler: (payload: ConfigChanged) => void) =>
  listen<ConfigChanged>("config-changed", (e) => handler(e.payload));
