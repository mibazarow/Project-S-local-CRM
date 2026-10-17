import { useEffect, useState, type ReactNode } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, SearchHit } from "../../api/types";
import { toMessage } from "./fileOps";

interface Props {
  projectId: number;
  query: string;
  onSelect: (entry: FsEntry) => void;
  onOpenTask: (taskId: number | null) => void;
}

const MATCH_LABELS: Record<string, string> = { fts: "слова", semantic: "смысл" };
const STATUS_LABELS: Record<string, string> = { new: "Новые", progress: "В работе", done: "Готово" };

// FTS5 snippet markers ("<<match>>") become <mark> elements.
export function renderSnippet(snippet: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const re = /<<([^>]*)>>/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(snippet))) {
    if (m.index > last) nodes.push(snippet.slice(last, m.index));
    nodes.push(<mark key={m.index}>{m[1]}</mark>);
    last = m.index + m[0].length;
  }
  nodes.push(snippet.slice(last));
  return nodes;
}

export default function SearchResults({ projectId, query, onSelect, onOpenTask }: Props) {
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const result = await ipc.searchProject(projectId, query);
        if (!cancelled) setHits(result);
      } catch (e) {
        if (!cancelled) setError(toMessage(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [projectId, query]);

  // A hit carries no fs metadata; resolve the real FsEntry through the tree's
  // own listing so FilePreview gets correct flags and size.
  const openHit = async (hit: SearchHit) => {
    const slash = hit.relPath.lastIndexOf("/");
    const dir = slash === -1 ? "" : hit.relPath.slice(0, slash);
    try {
      const entries = await ipc.fsListDir(projectId, dir);
      const entry = entries.find((e) => e.relPath === hit.relPath);
      if (entry) onSelect(entry);
      else setError(`Файл «${hit.name}» больше недоступен`);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  if (loading) return <div className="empty-note">Поиск…</div>;
  if (error) return <div className="tree-error">{error}</div>;
  if (hits.length === 0) return <div className="empty-note">Ничего не найдено</div>;

  return (
    <div className="search-results">
      {hits.map((hit) => {
        const isTask = hit.kind === "task";
        return (
          <div
            className={isTask ? "search-hit search-hit-task" : "search-hit"}
            key={isTask ? `task:${hit.taskId}` : hit.relPath}
            onClick={() => (isTask ? onOpenTask(hit.taskId ?? null) : void openHit(hit))}
          >
            <div className="search-hit-head">
              <span className="search-hit-name">{hit.name}</span>
              {!isTask && <span className="search-hit-path">{hit.relPath}</span>}
              <span className="search-hit-badges">
                {isTask && hit.status && (
                  <span className={`badge st-${hit.status}`}>
                    {STATUS_LABELS[hit.status] ?? hit.status}
                  </span>
                )}
                {isTask && hit.overdue && <span className="badge st-overdue">просрочено</span>}
                {hit.matchTypes.map((t) => (
                  <span className="badge" key={t}>
                    {MATCH_LABELS[t] ?? t}
                  </span>
                ))}
              </span>
            </div>
            {!isTask && hit.snippet && (
              <div className="search-snippet">{renderSnippet(hit.snippet)}</div>
            )}
          </div>
        );
      })}
    </div>
  );
}
