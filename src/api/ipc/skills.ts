// Skills management IPC (SPEC 6.2).
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ImportResult, SkillInfo } from "../types";

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

export const onSkillsChanged = (handler: () => void) =>
  listen<unknown>("skills-changed", () => handler());
