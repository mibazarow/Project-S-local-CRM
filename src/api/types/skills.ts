// skills::SkillInfo and skills::ImportResult.

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
