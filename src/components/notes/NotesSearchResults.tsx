import * as ipc from "../../api/ipc";
import type { FsEntry, SearchHit } from "../../api/types";
import { toMessage } from "../chat/chatShared";
import { renderSnippet } from "../files/SearchResults";
import { useSearchHits } from "../../hooks/useSearchHits";
import { resolveEntry } from "../../shared/resolveEntry";

interface Props {
  query: string;
  treeVersion: number;
  onOpen: (entry: FsEntry) => void;
}

export default function NotesSearchResults({ query, treeVersion, onOpen }: Props) {
  const { hits, loading, error, setError } = useSearchHits(
    () => ipc.searchNotes(query),
    [query, treeVersion],
  );

  // A hit carries no fs metadata; resolve the real FsEntry through the
  // folder listing so the editor gets correct flags.
  const openHit = async (hit: SearchHit) => {
    try {
      const entry = await resolveEntry(hit.relPath, (dir) => ipc.notesList(dir));
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
