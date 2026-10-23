use crate::error::Result;
use rusqlite::Connection;
use std::collections::HashMap;

use super::text::fallback_snippet;
use super::types::SearchHit;

// Batched hit enrichment (S1.3): one mtime query and one content query per
// chunk of file hits instead of up to two single-row queries per hit. Chunked
// to stay clear of SQLite's max-variable-number limit. Task hits (empty
// rel_path) are skipped: the old per-hit lookups found no row for them, so
// they keep modified = 0 and their own snippet. Missing rows keep the old
// fallbacks: mtime 0, empty content.
const ENRICH_CHUNK: usize = 500;

pub(crate) fn enrich_hits(
    conn: &Connection,
    project_id: i64,
    hits: &mut [SearchHit],
    lowered: &[String],
) -> Result<()> {
    let rels: Vec<&str> = hits
        .iter()
        .filter(|h| !h.rel_path.is_empty())
        .map(|h| h.rel_path.as_str())
        .collect();
    if rels.is_empty() {
        return Ok(());
    }
    let mut mtimes: HashMap<String, i64> = HashMap::new();
    for chunk in rels.chunks(ENRICH_CHUNK) {
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        args.push(&project_id);
        args.extend(chunk.iter().map(|r| r as &dyn rusqlite::ToSql));
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT rel_path, mtime FROM indexed_files
             WHERE project_id = ?1 AND rel_path IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (rel, mtime) = row?;
            mtimes.insert(rel, mtime);
        }
    }

    // Content is only needed for hits without an FTS snippet (semantic-only
    // hits and name-column matches), same condition as the old per-hit leg.
    let need_content: Vec<&str> = hits
        .iter()
        .filter(|h| !h.rel_path.is_empty() && h.snippet.is_empty())
        .map(|h| h.rel_path.as_str())
        .collect();
    let mut contents: HashMap<String, Option<String>> = HashMap::new();
    let pid_text = project_id.to_string();
    for chunk in need_content.chunks(ENRICH_CHUNK) {
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        args.push(&pid_text);
        args.extend(chunk.iter().map(|r| r as &dyn rusqlite::ToSql));
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT rel_path, content FROM files_fts
             WHERE project_id = ?1 AND rel_path IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (rel, content) = row?;
            // or_insert: on duplicate FTS rows the first one wins, matching
            // the old query_row which took the first row of the scan.
            contents.entry(rel).or_insert(content);
        }
    }

    for hit in hits.iter_mut() {
        if hit.rel_path.is_empty() {
            continue;
        }
        hit.modified = mtimes.get(hit.rel_path.as_str()).copied().unwrap_or(0);
        if hit.snippet.is_empty() {
            let content = contents
                .get(hit.rel_path.as_str())
                .and_then(Option::as_deref)
                .unwrap_or("");
            hit.snippet = fallback_snippet(content, lowered);
        }
    }
    Ok(())
}
