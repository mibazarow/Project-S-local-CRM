// Last opened project per working dir: survives restarts and tab switches,
// separate key per workspace so project ids never leak across working dirs.
const lastProjectKey = (workingDir: string) =>
  `project-s:lastProject:${encodeURIComponent(workingDir)}`;

export function readLastProjectId(workingDir: string | null): number | null {
  if (!workingDir) return null;
  try {
    const raw = localStorage.getItem(lastProjectKey(workingDir));
    const id = raw === null ? NaN : Number(raw);
    return Number.isInteger(id) ? id : null;
  } catch {
    return null;
  }
}

export function writeLastProjectId(workingDir: string | null, id: number): void {
  if (!workingDir) return;
  try {
    localStorage.setItem(lastProjectKey(workingDir), String(id));
  } catch {
    // Storage unavailable: the selection simply won't persist across restarts.
  }
}
