import { useEffect, useState } from "react";
import * as ipc from "../../api/ipc";
import type { FsEntry, SearchHit } from "../../api/types";
import { toMessage } from "../chat/chatShared";
import { renderSnippet } from "../files/SearchResults";

interface Props {
  query: string;
  treeVersion: number;
  onOpen: (entry: FsEntry) => void;
}

export default function NotesSearchResults({ query, treeVersion, onOpen }: Props) {
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const result = await ipc.searchNotes(query);
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
  }, [query, treeVersion]);

  // A hit carries no fs metadata; resolve the real FsEntry through the
  // folder listing so the editor gets correct flags.
  const openHit = async (hit: SearchHit) => {
    const slash = hit.relPath.lastIndexOf("/");
    const dir = slash === -1 ? "" : hit.relPath.slice(0, slash);
    try {
      const entries = await ipc.notesList(dir);
      const entry = entries.find((e) => e.relPath === hit.relPath);
      if (entry) onOpen(entry);
      else setError(`Заметка «${hit.name}» больше недоступна`);
    } catch (e) {
      setError(toMessage(e));
    }
  };

  return (
    <div className="notes-search-results">
      {loading ? (
        <div className="empty-note">Поиск…</div>
      ) : error ? (
        <div className="tree-error">{error}</div>
      ) : hits.length === 0 ? (
        <div className="empty-note">Ничего не найдено</div>
      ) : (
        <div className="search-results">
          {hits.map((hit) => (
            <div
              className="search-hit"
              key={hit.relPath}
              onClick={() => void openHit(hit)}
            >
              <div className="search-hit-head">
                <span className="search-hit-name">{hit.name}</span>
                <span className="search-hit-path">{hit.relPath}</span>
                <span className="search-hit-badges">
                  {hit.matchTypes.map((t) => (
                    <span className="badge" key={t}>
                      {t === "fts" ? "слова" : t === "semantic" ? "смысл" : t}
                    </span>
                  ))}
                </span>
              </div>
              {hit.snippet && <div className="search-snippet">{renderSnippet(hit.snippet)}</div>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
