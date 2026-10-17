pub mod walk;

use crate::config::{self, ConfigKind, IndexingConfig};
use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::extract;
use crate::paths;
use crate::projects;
use crate::state::AppState;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStats {
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_removed: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub indexed_files: i64,
    pub last_indexed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexChanged {
    pub project_id: i64,
    pub stats: IndexStats,
}

// Opens its own connection so indexing never blocks the shared UI connection;
// WAL plus busy_timeout makes concurrent reads/writes safe.
pub fn index_project(state: &AppState, project_id: i64) -> Result<IndexStats> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let root = projects::project_root(&conn, &wd, project_id)?;
    let cfg = indexing_config(&wd)?;
    let embedder = embed::select_embedder(&cfg.embedding, &wd);
    index_root(&conn, project_id, &root, &cfg, embedder.as_ref())
}

pub fn index_all(state: &AppState) -> Result<Vec<(i64, IndexStats)>> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let cfg = indexing_config(&wd)?;
    let embedder = embed::select_embedder(&cfg.embedding, &wd);
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id")?;
        for row in stmt.query_map([], |r| r.get::<_, i64>(0))? {
            ids.push(row?);
        }
    }
    let mut out = Vec::new();
    for id in ids {
        match projects::project_root(&conn, &wd, id) {
            Ok(root) => out.push((id, index_root(&conn, id, &root, &cfg, embedder.as_ref())?)),
            Err(e) => out.push((
                id,
                IndexStats {
                    errors: vec![e.to_string()],
                    ..Default::default()
                },
            )),
        }
    }
    Ok(out)
}

pub fn index_status(state: &AppState, project_id: i64) -> Result<IndexStatus> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(AppError::Invalid(format!("project {project_id} not found")));
    }
    let indexed_files = conn.query_row(
        "SELECT count(*) FROM indexed_files WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    let last_indexed_at = conn.query_row(
        "SELECT max(indexed_at) FROM indexed_files WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    Ok(IndexStatus {
        indexed_files,
        last_indexed_at,
    })
}

pub(crate) fn indexing_config(wd: &Path) -> Result<IndexingConfig> {
    let value = config::read_config(wd, ConfigKind::Indexing)?;
    Ok(serde_json::from_value(value)?)
}

pub(crate) fn index_root(
    conn: &Connection,
    project_id: i64,
    root: &Path,
    cfg: &IndexingConfig,
    embedder: &dyn embed::Embedder,
) -> Result<IndexStats> {
    let tx = conn.unchecked_transaction()?;
    let mut stats = IndexStats::default();
    let known = known_rows(&tx, project_id)?;
    let mut vec_rows = vec_rows(&tx, project_id)?;
    let mut indexed_rels: HashSet<String> = HashSet::new();
    let embedding_on = embedder.backend_id() != "noop";

    for file in walk::walk(root, cfg) {
        stats.files_seen += 1;
        if !extract::is_supported(&file.path) {
            stats.files_skipped += 1;
            continue;
        }
        if file.size > cfg.max_file_size_bytes {
            stats.files_skipped += 1;
            continue;
        }
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some((size, mtime)) = known.get(&file.rel) {
            if *size == file.size as i64 && *mtime == file.mtime {
                indexed_rels.insert(file.rel.clone());
                stats.files_skipped += 1;
                // Backfill vectors when only the backend or dim changed.
                if embedding_on
                    && needs_reembed(&vec_rows, &file.rel, embedder.backend_id(), embedder.dim())
                {
                    if let Some(text) = fts_content(&tx, project_id, &file.rel) {
                        if let Err(e) = update_vectors(
                            &tx, embedder, &mut vec_rows, project_id, &file.rel, &name, &text,
                        ) {
                            stats.errors.push(format!("{}: embed: {e}", file.rel));
                        }
                    }
                }
                continue;
            }
        }
        match extract::extract_text(&file.path, cfg.max_text_chars as usize) {
            Ok(text) => {
                upsert_file(&tx, project_id, &file, &name, &text)?;
                indexed_rels.insert(file.rel.clone());
                stats.files_indexed += 1;
                if embedding_on {
                    if let Err(e) = update_vectors(
                        &tx, embedder, &mut vec_rows, project_id, &file.rel, &name, &text,
                    ) {
                        stats.errors.push(format!("{}: embed: {e}", file.rel));
                    }
                }
            }
            Err(e) => stats.errors.push(format!("{}: {e}", file.rel)),
        }
    }

    stats.files_removed = remove_missing(&tx, project_id, &indexed_rels)?;
    tx.commit()?;
    Ok(stats)
}

fn known_rows(conn: &Connection, project_id: i64) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT rel_path, size, mtime FROM indexed_files WHERE project_id = ?1")?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

fn upsert_file(
    conn: &Connection,
    project_id: i64,
    file: &walk::WalkedFile,
    name: &str,
    text: &str,
) -> Result<()> {
    let kind = extract::kind(&file.path).unwrap_or("text");
    conn.execute(
        "INSERT INTO indexed_files (project_id, rel_path, name, kind, size, mtime, content_hash, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now','localtime'))
         ON CONFLICT(project_id, rel_path) DO UPDATE SET
             name = excluded.name, kind = excluded.kind, size = excluded.size,
             mtime = excluded.mtime, content_hash = excluded.content_hash,
             indexed_at = excluded.indexed_at",
        params![project_id, file.rel, name, kind, file.size as i64, file.mtime, content_hash(text)],
    )?;
    conn.execute(
        "DELETE FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
        params![project_id.to_string(), file.rel],
    )?;
    conn.execute(
        "INSERT INTO files_fts (rel_path, project_id, name, content) VALUES (?1, ?2, ?3, ?4)",
        params![file.rel, project_id.to_string(), name, text],
    )?;
    Ok(())
}

// Rows left over from files that were deleted, renamed, or turned unsupported.
fn remove_missing(conn: &Connection, project_id: i64, keep: &HashSet<String>) -> Result<u64> {
    let mut stale = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT rel_path FROM indexed_files WHERE project_id = ?1")?;
        for row in stmt.query_map(params![project_id], |r| r.get::<_, String>(0))? {
            let rel = row?;
            if !keep.contains(&rel) {
                stale.push(rel);
            }
        }
    }
    for rel in &stale {
        conn.execute(
            "DELETE FROM indexed_files WHERE project_id = ?1 AND rel_path = ?2",
            params![project_id, rel],
        )?;
        conn.execute(
            "DELETE FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
            params![project_id.to_string(), rel],
        )?;
        conn.execute(
            "DELETE FROM vec_items WHERE rowid IN (SELECT vec_rowid FROM file_vectors WHERE project_id = ?1 AND rel_path = ?2 AND vec_rowid IS NOT NULL)",
            params![project_id, rel],
        )?;
        conn.execute(
            "DELETE FROM file_vectors WHERE project_id = ?1 AND rel_path = ?2",
            params![project_id, rel],
        )?;
    }
    Ok(stale.len() as u64)
}

// (rel_path, kind) -> (text_hash, backend, dim, vec_rowid)
type VecRow = (Option<String>, String, i64, Option<i64>);

fn vec_rows(conn: &Connection, project_id: i64) -> Result<HashMap<(String, String), VecRow>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, kind, text_hash, backend, dim, vec_rowid FROM file_vectors WHERE project_id = ?1",
    )?;
    let rows = stmt.query_map(params![project_id], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            (
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

fn needs_reembed(
    vec_rows: &HashMap<(String, String), VecRow>,
    rel: &str,
    backend: &str,
    dim: usize,
) -> bool {
    ["text", "name"].iter().any(|kind| {
        match vec_rows.get(&(rel.to_owned(), kind.to_string())) {
            Some((_, stored_backend, stored_dim, Some(_))) => {
                *stored_backend != backend || *stored_dim != dim as i64
            }
            _ => true,
        }
    })
}

fn fts_content(conn: &Connection, project_id: i64, rel: &str) -> Option<String> {
    conn.query_row(
        "SELECT content FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
        params![project_id.to_string(), rel],
        |r| r.get(0),
    )
    .ok()
}

// Stores one text vector and one name vector per file, skipping kinds whose
// hash, backend and dim are unchanged. Embedder failures bubble up to the
// caller, which records them in stats.errors without aborting the run.
fn update_vectors(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    vec_rows: &mut HashMap<(String, String), VecRow>,
    project_id: i64,
    rel: &str,
    name: &str,
    text: &str,
) -> Result<()> {
    let dim = embedder.dim();
    let backend = embedder.backend_id();
    let docs: [(&'static str, String, String); 2] = [
        ("text", content_hash(text), embed::doc_prefix(name, text)),
        (
            "name",
            content_hash(&embed::name_prefix(name)),
            embed::name_prefix(name),
        ),
    ];
    let mut pending: Vec<(&'static str, String, String)> = Vec::new();
    for (kind, hash, input) in docs {
        if let Some((Some(stored_hash), stored_backend, stored_dim, _)) =
            vec_rows.get(&(rel.to_owned(), kind.to_owned()))
        {
            if *stored_hash == hash && *stored_backend == backend && *stored_dim == dim as i64 {
                continue;
            }
        }
        pending.push((kind, hash, input));
    }
    if pending.is_empty() {
        return Ok(());
    }
    let inputs: Vec<String> = pending.iter().map(|(_, _, input)| input.clone()).collect();
    let mut vectors = embedder.embed_texts(&inputs)?;
    for ((kind, hash, _), mut vector) in pending.drain(..).zip(vectors.drain(..)) {
        if vector.len() < dim {
            return Err(AppError::Invalid(format!(
                "embedder {backend} returned {} dims, need {dim}",
                vector.len()
            )));
        }
        embed::truncate_renorm(&mut vector, dim);
        let key = (rel.to_owned(), kind.to_owned());
        if let Some((_, _, _, Some(rowid))) = vec_rows.get(&key) {
            conn.execute("DELETE FROM vec_items WHERE rowid = ?1", params![rowid])?;
        }
        let blob: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (?1, ?2)",
            params![project_id, blob],
        )?;
        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_vectors (project_id, rel_path, kind, dim, backend, text_hash, vec_rowid, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now','localtime'))
             ON CONFLICT(project_id, rel_path, kind) DO UPDATE SET
                 dim = excluded.dim, backend = excluded.backend, text_hash = excluded.text_hash,
                 vec_rowid = excluded.vec_rowid, updated_at = excluded.updated_at",
            params![project_id, rel, kind, dim as i64, backend, hash, rowid],
        )?;
        vec_rows.insert(key, (Some(hash), backend.to_owned(), dim as i64, Some(rowid)));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticHit {
    pub rel_path: String,
    pub name: String,
    pub score: f64,
}

// Noop embedder yields an empty list, not an error: FTS search still works.
pub fn semantic_search(
    state: &AppState,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SemanticHit>> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let cfg = indexing_config(&wd)?;
    let embedder = embed::select_embedder(&cfg.embedding, &wd);
    if embedder.backend_id() == "noop" {
        return Ok(Vec::new());
    }
    semantic_search_with(
        &conn,
        embedder.as_ref(),
        cfg.embedding.name_weight,
        project_id,
        query,
        limit,
    )
}

pub(crate) fn semantic_search_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    name_weight: f32,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SemanticHit>> {
    let mut query_vector = embedder
        .embed_texts(&[embed::query_prefix(query)])?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
    if query_vector.len() < embedder.dim() {
        return Err(AppError::Invalid(
            "embedder returned too few dims".into(),
        ));
    }
    embed::truncate_renorm(&mut query_vector, embedder.dim());
    let blob: Vec<u8> = query_vector.iter().flat_map(|x| x.to_le_bytes()).collect();
    // two vectors per file, so fetch twice the candidates
    let k = (limit.saturating_mul(2)).max(1) as i64;
    let mut per_file: HashMap<String, (Option<f32>, Option<f32>)> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT q.distance, f.rel_path, f.kind
             FROM (SELECT rowid, distance FROM vec_items WHERE embedding MATCH ?1 AND k = ?2 AND project_id = ?3) q
             JOIN file_vectors f ON f.vec_rowid = q.rowid",
        )?;
        let rows = stmt.query_map(params![blob, k, project_id], |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (distance, rel, kind) = row?;
            // unit vectors: cosine similarity = 1 - L2^2 / 2
            let d = distance as f32;
            let sim = (1.0 - d * d / 2.0).clamp(0.0, 1.0);
            let entry = per_file.entry(rel).or_default();
            if kind == "text" {
                entry.0 = Some(sim);
            } else {
                entry.1 = Some(sim);
            }
        }
    }
    let mut hits = Vec::new();
    for (rel, (text_sim, name_sim)) in per_file {
        let name: String = conn
            .query_row(
                "SELECT name FROM indexed_files WHERE project_id = ?1 AND rel_path = ?2",
                params![project_id, rel],
                |r| r.get(0),
            )
            .unwrap_or_default();
        hits.push(SemanticHit {
            rel_path: rel,
            name,
            score: embed::blend(text_sim, name_sim, name_weight) as f64,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

// FNV-1a 64-bit, enough to spot content changes without a hash crate.
fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (PathBuf, Connection, i64, AppState) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        (wd, conn, dto.id, state)
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
    }

    fn fts_rel_paths(conn: &Connection, project_id: i64, query: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT rel_path FROM files_fts WHERE files_fts MATCH ?1 AND project_id = ?2")
            .unwrap();
        stmt.query_map(params![query, project_id.to_string()], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn bundled_sqlite_supports_fts5() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE VIRTUAL TABLE t USING fts5(content, tokenize='unicode61')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO t (content) VALUES ('привет мир')", [])
            .unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'привет'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn schema_v2_creates_index_tables_and_is_idempotent() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::run_migrations(&conn).unwrap();
        crate::db::schema::run_migrations(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('indexed_files','files_fts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn index_project_indexes_files_and_cyrillic_search_finds_them() {
        let (wd, conn, id, state) = setup("idx-basic");
        fs::create_dir_all(root(&wd).join("notes")).unwrap();
        fs::write(root(&wd).join("notes/записи.md"), "Клиент example.ru оплатил счёт").unwrap();
        fs::write(root(&wd).join(paths::DATAS_MD), "доступ: ftp.example.ru").unwrap();

        let stats = index_project(&state, id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.files_indexed, 2);
        assert!(stats.errors.is_empty());

        let rows: i64 = conn
            .query_row("SELECT count(*) FROM indexed_files WHERE project_id = ?1", params![id], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(fts_rel_paths(&conn, id, "оплатил"), ["notes/записи.md"]);
        assert_eq!(fts_rel_paths(&conn, id, "ftp"), ["datas.md"]);

        let status = index_status(&state, id).unwrap();
        assert_eq!(status.indexed_files, 2);
        assert!(status.last_indexed_at.is_some());
        assert!(matches!(index_status(&state, 999), Err(AppError::Invalid(_))));

        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn second_run_skips_unchanged_reindexes_modified_removes_deleted() {
        let (wd, conn, id, state) = setup("idx-inc");
        fs::write(root(&wd).join("a.txt"), "раз").unwrap();
        fs::write(root(&wd).join("b.md"), "два").unwrap();

        let first = index_project(&state, id).unwrap();
        assert_eq!(first.files_indexed, 3); // datas.md + a.txt + b.md

        let second = index_project(&state, id).unwrap();
        assert_eq!(second.files_indexed, 0);
        assert_eq!(second.files_skipped, 3);

        fs::write(root(&wd).join("a.txt"), "раз изменённый текст").unwrap();
        fs::remove_file(root(&wd).join("b.md")).unwrap();
        let third = index_project(&state, id).unwrap();
        assert_eq!(third.files_indexed, 1);
        assert_eq!(third.files_removed, 1);

        assert!(fts_rel_paths(&conn, id, "изменённый") == ["a.txt"]);
        assert!(fts_rel_paths(&conn, id, "два").is_empty());
        let rels: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT rel_path FROM indexed_files WHERE project_id = ?1 ORDER BY rel_path")
                .unwrap();
            stmt.query_map(params![id], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(rels, ["a.txt", "datas.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn unsupported_binary_and_broken_files_are_skipped_or_recorded_without_abort() {
        let (wd, conn, id, state) = setup("idx-bad");
        fs::write(root(&wd).join("good.md"), "хорошо").unwrap();
        fs::write(root(&wd).join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        fs::write(root(&wd).join("bin.txt"), [b'o', b'k', 0, 1]).unwrap();
        fs::write(root(&wd).join("broken.docx"), b"PK\x03\x04not a zip").unwrap();

        let stats = index_project(&state, id).unwrap();
        assert_eq!(stats.files_indexed, 2); // good.md + datas.md
        assert_eq!(stats.files_skipped, 1); // png has an unsupported extension
        assert_eq!(stats.errors.len(), 2); // binary txt + broken docx, run continued
        assert!(stats.errors.iter().any(|e| e.contains("broken.docx")));
        assert!(stats.errors.iter().any(|e| e.contains("bin.txt")));
        assert_eq!(fts_rel_paths(&conn, id, "хорошо"), ["good.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn exclusions_size_and_char_limits_are_honored() {
        let (wd, _conn, id, state) = setup("idx-limits");
        fs::create_dir_all(paths::config_dir(&wd)).unwrap();
        fs::write(
            paths::config_dir(&wd).join(paths::CONFIG_INDEXING),
            r#"{ "maxFileSizeBytes": 50, "maxTextChars": 10, "excludeDirs": [".git", "vendor"] }"#,
        )
        .unwrap();
        fs::create_dir_all(root(&wd).join("vendor")).unwrap();
        fs::write(root(&wd).join("vendor/skip.md"), "нельзя").unwrap();
        fs::write(root(&wd).join("big.txt"), "x".repeat(100)).unwrap();
        fs::write(root(&wd).join("long.md"), "x".repeat(40)).unwrap();

        let stats = index_project(&state, id).unwrap();
        assert_eq!(stats.files_indexed, 2); // datas.md + long.md
        assert_eq!(stats.files_skipped, 1); // big.txt over size limit
        assert_eq!(stats.files_seen, 3); // vendor/skip.md never seen

        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let content: String = conn
            .query_row("SELECT content FROM files_fts WHERE rel_path = 'long.md'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(content.chars().count(), 10); // truncated to maxTextChars
        let missing: i64 = conn
            .query_row("SELECT count(*) FROM indexed_files WHERE rel_path IN ('vendor/skip.md','big.txt')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn index_all_covers_every_project() {
        let (wd, conn, id, state) = setup("idx-all");
        let other = projects::create_project(&conn, &wd, "second").unwrap();
        fs::write(paths::projects_dir(&wd).join("second/note.md"), "второй проект").unwrap();

        let results = index_all(&state).unwrap();
        assert_eq!(results.len(), 2);
        let total_indexed: u64 = results.iter().map(|(_, s)| s.files_indexed).sum();
        assert_eq!(total_indexed, 3); // datas.md x2 + note.md
        assert_eq!(fts_rel_paths(&conn, other.id, "второй"), ["note.md"]);
        assert_eq!(fts_rel_paths(&conn, id, "второй").len(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    // Deterministic stand-in: byte-bucket counts, no network.
    struct FakeEmbedder {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeEmbedder {
        fn new() -> Self {
            FakeEmbedder {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl embed::Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 768];
                    for b in t.bytes() {
                        v[b as usize % 768] += 1.0;
                    }
                    v
                })
                .collect())
        }
        fn dim(&self) -> usize {
            embed::EMBED_DIM
        }
        fn backend_id(&self) -> &'static str {
            "fake"
        }
    }

    fn vector_count(conn: &Connection, project_id: i64) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM file_vectors WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn vec_item_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM vec_items", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn fake_embedder_stores_two_vectors_per_file_and_stays_incremental() {
        let (wd, conn, id, _state) = setup("idx-vec");
        fs::write(root(&wd).join("a.txt"), "раз").unwrap();
        let fake = FakeEmbedder::new();
        let cfg = IndexingConfig::default();

        index_root(&conn, id, &root(&wd), &cfg, &fake).unwrap();
        assert_eq!(vector_count(&conn, id), 4); // datas.md + a.txt, text + name each
        assert_eq!(vec_item_count(&conn), 4);
        let calls = fake.calls();
        assert_eq!(calls, 2);

        index_root(&conn, id, &root(&wd), &cfg, &fake).unwrap();
        assert_eq!(fake.calls(), calls); // unchanged files: no recompute
        assert_eq!(vector_count(&conn, id), 4);

        fs::write(root(&wd).join("a.txt"), "раз").unwrap(); // same content, new mtime
        index_root(&conn, id, &root(&wd), &cfg, &fake).unwrap();
        assert_eq!(fake.calls(), calls); // text_hash unchanged: no recompute

        fs::remove_file(root(&wd).join("a.txt")).unwrap();
        index_root(&conn, id, &root(&wd), &cfg, &fake).unwrap();
        assert_eq!(vector_count(&conn, id), 2);
        assert_eq!(vec_item_count(&conn), 2); // vec_items cleaned up too
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_search_with_fake_embedder_ranks_matching_file_first() {
        let (wd, conn, id, _state) = setup("idx-sem");
        fs::write(root(&wd).join("cats.md"), "cats cats cats").unwrap();
        fs::write(root(&wd).join("dogs.md"), "dogs dogs dogs").unwrap();
        let fake = FakeEmbedder::new();
        index_root(&conn, id, &root(&wd), &IndexingConfig::default(), &fake).unwrap();

        let hits = semantic_search_with(&conn, &fake, 0.3, id, "cats", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].rel_path, "cats.md");
        assert_eq!(hits[0].name, "cats.md");
        assert!(hits[0].score > hits[hits.len() - 1].score);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_search_returns_empty_list_with_noop_backend() {
        let (wd, _conn, id, state) = setup("idx-noop");
        fs::write(root(&wd).join("a.txt"), "раз").unwrap();
        index_project(&state, id).unwrap(); // default config: ort, but no model files here -> noop
        let hits = semantic_search(&state, id, "раз", 5).unwrap();
        assert!(hits.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
}
