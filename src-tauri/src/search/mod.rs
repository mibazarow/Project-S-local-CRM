use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::index;
use crate::paths;
use crate::state::AppState;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashMap;

// Reciprocal rank fusion: each leg contributes 1 / (RRF_K + rank), rank starts at 1.
const RRF_K: f64 = 60.0;
const SNIPPET_WIDTH: usize = 200;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub rel_path: String,
    pub name: String,
    // "file" or "task"; task hits carry an empty rel_path.
    pub kind: String,
    pub score: f64,
    pub snippet: String,
    pub match_types: Vec<String>,
    // File mtime in epoch milliseconds, from indexed_files (0 for task hits).
    pub modified: i64,
    // Task hits (SPEC 4.4): identify the task row for the UI.
    pub task_id: Option<i64>,
    pub status: Option<String>,
    pub overdue: Option<bool>,
}

// Hybrid per-project search: FTS5 keyword leg plus the vector semantic leg
// over files and tasks (closed tasks included), merged with RRF. An empty or
// punctuation-only query yields an empty list.
pub fn search_project(
    state: &AppState,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    search_with(
        &conn,
        embedder.as_ref(),
        embed::NAME_WEIGHT,
        project_id,
        query,
        limit,
    )
}

fn search_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    name_weight: f32,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let terms = tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let match_query = fts_match_query(&terms);
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let fts_rows = fts_search(conn, project_id, &match_query, limit)?;
    let task_rows = task_fts_search(conn, project_id, &match_query, limit)?;
    // Noop embedder: skip the semantic legs entirely, FTS-only stays offline.
    let (semantic, task_semantic) = if embedder.backend_id() == "noop" {
        (Vec::new(), Vec::new())
    } else {
        (
            index::semantic_search_with(conn, embedder, name_weight, project_id, query, limit)?,
            task_semantic_hits(conn, embedder, project_id, query, limit)?,
        )
    };

    let mut merged: HashMap<String, SearchHit> = HashMap::new();
    for (rank, (rel, name, snippet)) in fts_rows.into_iter().enumerate() {
        let hit = merged.entry(rel.clone()).or_insert_with(|| SearchHit {
            rel_path: rel,
            name,
            kind: "file".into(),
            score: 0.0,
            snippet,
            match_types: Vec::new(),
            modified: 0,
            task_id: None,
            status: None,
            overdue: None,
        });
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("fts".into());
    }
    for (rank, sem) in semantic.into_iter().enumerate() {
        let hit = merged
            .entry(sem.rel_path.clone())
            .or_insert_with(|| SearchHit {
                rel_path: sem.rel_path,
                name: sem.name,
                kind: "file".into(),
                score: 0.0,
                snippet: String::new(),
                match_types: Vec::new(),
                modified: 0,
                task_id: None,
                status: None,
                overdue: None,
            });
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("semantic".into());
    }
    // Task legs merge into the same RRF ranking, keyed by task id.
    let mut merged_tasks: HashMap<i64, SearchHit> = HashMap::new();
    for (rank, row) in task_rows.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("fts".into());
    }
    for (rank, row) in task_semantic.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("semantic".into());
    }

    let mut hits: Vec<SearchHit> = merged.into_values().chain(merged_tasks.into_values()).collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    hits.truncate(limit);

    for hit in &mut hits {
        hit.modified = file_mtime(conn, project_id, &hit.rel_path);
        if hit.snippet.is_empty() {
            let content = file_content(conn, project_id, &hit.rel_path).unwrap_or_default();
            hit.snippet = fallback_snippet(&content, &lowered);
        }
    }
    Ok(hits)
}

// Task row joined into search: status/overdue come from tasks, so closed
// tasks are included and status/date filtering stays possible via the join.
#[derive(Debug, Clone)]
struct TaskRow {
    id: i64,
    title: String,
    status: String,
    comment: Option<String>,
    tags: String,
    overdue: bool,
}

const TASK_COLS: &str = "tasks.id, tasks.title, tasks.status, tasks.comment, tasks.tags, \
     (tasks.due_date IS NOT NULL AND tasks.due_date < date('now','localtime') AND tasks.status <> 'done')";

fn task_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get(0)?,
        title: row.get(1)?,
        status: row.get(2)?,
        comment: row.get(3)?,
        tags: row.get(4)?,
        overdue: row.get::<_, i64>(5)? != 0,
    })
}

fn task_fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_COLS}
         FROM tasks_fts JOIN tasks ON tasks.id = tasks_fts.rowid
         WHERE tasks_fts MATCH ?1 AND tasks.project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    ))?;
    let rows = stmt.query_map(params![match_query, project_id, limit as i64], task_row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Kanban-scale task counts: embed every task text of the project at query
// time instead of storing task vectors — always fresh, no sync or backfill
// burden, and the Noop default keeps this leg off entirely.
fn task_semantic_hits(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_COLS} FROM tasks WHERE tasks.project_id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![project_id], task_row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let dim = embedder.dim();
    let mut query_vector = embedder
        .embed_texts(&[embed::query_prefix(query)])?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
    embed::truncate_renorm(&mut query_vector, dim);
    let docs: Vec<String> = rows
        .iter()
        .map(|t| embed::doc_prefix(&t.title, t.comment.as_deref().unwrap_or("")))
        .collect();
    let mut scored: Vec<(f64, TaskRow)> = rows
        .into_iter()
        .zip(embedder.embed_texts(&docs)?)
        .map(|(row, mut vector)| {
            embed::truncate_renorm(&mut vector, dim);
            // unit vectors: cosine similarity = dot product
            let sim: f32 = query_vector.iter().zip(&vector).map(|(a, b)| a * b).sum();
            (sim as f64, row)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, row)| row).collect())
}

// Contentless FTS tables cannot snippet, so the task snippet is built around
// the query terms from the task's own title/comment/tags.
fn task_hit(row: &TaskRow, lowered: &[String]) -> SearchHit {
    let mut text = row.title.clone();
    if let Some(comment) = &row.comment {
        text.push(' ');
        text.push_str(comment);
    }
    text.push(' ');
    text.push_str(&row.tags);
    SearchHit {
        rel_path: String::new(),
        name: row.title.clone(),
        kind: "task".into(),
        score: 0.0,
        snippet: fallback_snippet(&text, lowered),
        match_types: Vec::new(),
        modified: 0,
        task_id: Some(row.id),
        status: Some(row.status.clone()),
        overdue: Some(row.overdue),
    }
}

// Splits user input into plain word tokens; FTS operators ("*", quotes, NEAR,
// OR) never survive as syntax, only as word characters. Shared with memory.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

// Each token becomes a quoted phrase (tokens contain no quotes after
// tokenizing), joined with spaces for implicit AND.
pub(crate) fn fts_match_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, name, snippet(files_fts, 3, '<<', '>>', '…', 12)
         FROM files_fts
         WHERE files_fts MATCH ?1 AND project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![match_query, project_id.to_string(), limit as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn file_mtime(conn: &Connection, project_id: i64, rel: &str) -> i64 {
    conn.query_row(
        "SELECT mtime FROM indexed_files WHERE project_id = ?1 AND rel_path = ?2",
        params![project_id, rel],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn file_content(conn: &Connection, project_id: i64, rel: &str) -> Option<String> {
    conn.query_row(
        "SELECT content FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
        params![project_id.to_string(), rel],
        |r| r.get(0),
    )
    .ok()
}

// Short window around the first query token found in the stored content, else
// a plain prefix. Used for semantic-only hits and FTS hits whose match was in
// the name column (empty content snippet).
fn fallback_snippet(content: &str, terms_lower: &[String]) -> String {
    let chars: Vec<char> = content.chars().collect();
    let lower = content.to_lowercase();
    let best = terms_lower
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .map(|byte_pos| lower[..byte_pos].chars().count())
        .unwrap_or(0);
    let start = best.saturating_sub(SNIPPET_WIDTH / 3);
    let end = (start + SNIPPET_WIDTH).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexingConfig;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::tasks;
    use std::fs;
    use std::path::{Path, PathBuf};

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

    // Deterministic stand-in: byte-bucket counts, no network.
    struct FakeEmbedder;

    impl embed::Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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

    fn rels(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.rel_path.clone()).collect()
    }

    #[test]
    fn fts_only_finds_cyrillic_word_and_snippet_wraps_match() {
        let (wd, _conn, id, state) = setup("search-fts");
        fs::create_dir_all(root(&wd).join("notes")).unwrap();
        fs::write(
            root(&wd).join("notes/записи.md"),
            "Клиент example.ru оплатил счёт за редизайн",
        )
        .unwrap();
        index::index_project(&state, id).unwrap(); // default config: noop embedder

        let hits = search_project(&state, id, "оплатил", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.rel_path, "notes/записи.md");
        assert_eq!(hit.name, "записи.md");
        assert_eq!(hit.kind, "file");
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("<<оплатил>>"), "{:?}", hit.snippet);
        assert!(hit.modified > 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fts_injection_attempts_are_treated_as_plain_tokens() {
        let (wd, _conn, id, state) = setup("search-inject");
        fs::write(root(&wd).join("near.md"), "near or far").unwrap();
        fs::write(root(&wd).join("only.md"), "near here").unwrap();
        fs::write(root(&wd).join("foo.md"), "foo bar").unwrap();
        index::index_project(&state, id).unwrap();

        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced", "NEAR", "foo*"] {
            assert!(
                search_project(&state, id, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        let query = |q: &str| rels(&search_project(&state, id, q, 10).unwrap());
        // operators become plain words: AND of both tokens, not an OR operator
        assert_eq!(query("NEAR OR"), ["near.md"]);
        assert_eq!(query("near OR"), ["near.md"]);
        assert_eq!(query("near OR foo"), Vec::<String>::new());
        // pure syntax yields no tokens, hence no results
        assert!(query("*").is_empty());
        assert!(query("\"").is_empty());
        assert!(query("\"unbalanced").is_empty());
        // "*" is stripped, the plain token still matches
        assert_eq!(query("foo"), ["foo.md"]);
        assert_eq!(query("foo*"), ["foo.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn hybrid_adds_semantic_only_hit_and_merges_match_types() {
        let (wd, conn, id, _state) = setup("search-hybrid");
        fs::write(root(&wd).join("кошки.md"), "кошки кошки кошки").unwrap();
        fs::write(root(&wd).join("самолет.md"), "самолет летит над морем").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        // differently worded query: FTS finds nothing, semantic finds the file
        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["semantic"]);
        assert!(!hits[0].snippet.is_empty());

        // literal word: both legs hit the same file, both matchTypes kept
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["fts", "semantic"]);

        // RRF ordering is deterministic across runs
        let again = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(rels(&hits), rels(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn empty_query_returns_empty_and_noop_backend_stays_fts_only() {
        let (wd, _conn, id, state) = setup("search-empty");
        fs::write(root(&wd).join("a.txt"), "кошки").unwrap();
        index::index_project(&state, id).unwrap();

        assert!(search_project(&state, id, "", 10).unwrap().is_empty());
        assert!(search_project(&state, id, "   ", 10).unwrap().is_empty());
        let hits = search_project(&state, id, "кошки", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits.iter().all(|h| h.match_types == ["fts"]));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn limit_is_respected_and_results_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        fs::write(
            paths::projects_dir(&wd).join("other/x.md"),
            "слово из другого проекта",
        )
        .unwrap();
        index::index_project(&state, id).unwrap();
        index::index_project(&state, other.id).unwrap();

        assert_eq!(search_project(&state, id, "слово", 2).unwrap().len(), 2);
        let mut all = rels(&search_project(&state, id, "слово", 100).unwrap());
        all.sort();
        assert_eq!(all, ["a.md", "b.md", "c.md"]);

        let other_hits = search_project(&state, other.id, "слово", 10).unwrap();
        assert_eq!(rels(&other_hits), ["x.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_fts_hit_includes_closed_task_and_carries_task_fields() {
        let (wd, conn, id, state) = setup("search-task-closed");
        let t = tasks::task_create(&conn, id, "Сделать редизайн главной", None).unwrap();
        tasks::task_move(&conn, t.id, "done", 1.0).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.kind, "task");
        assert_eq!(hit.rel_path, "");
        assert_eq!(hit.name, "Сделать редизайн главной");
        assert_eq!(hit.task_id, Some(t.id));
        assert_eq!(hit.status.as_deref(), Some("done"));
        assert_eq!(hit.overdue, Some(false));
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("редизайн"), "{:?}", hit.snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_title_comment_and_tags_are_all_searchable() {
        let (wd, conn, id, state) = setup("search-task-fields");
        let t = tasks::task_create(&conn, id, "Задача про баннер", None).unwrap();
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                comment: Some(Some("нужен макет логотипа".into())),
                tags: Some(vec!["полиграфия".into()]),
                ..Default::default()
            },
        )
        .unwrap();

        for query in ["баннер", "логотипа", "полиграфия"] {
            let hits = search_project(&state, id, query, 10).unwrap();
            assert_eq!(hits.len(), 1, "query {query:?}");
            assert_eq!(hits[0].kind, "task");
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_mutations_keep_search_in_sync() {
        let (wd, conn, id, state) = setup("search-task-sync");
        let find = |q: &str| {
            search_project(&state, id, q, 10)
                .unwrap()
                .iter()
                .any(|h| h.kind == "task")
        };
        let t = tasks::task_create(&conn, id, "старое слово", None).unwrap();
        assert!(find("старое"), "created task must be findable");
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                title: Some("новое слово".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(find("новое"), "updated text must be findable");
        assert!(!find("старое"), "old text must be gone after update");
        tasks::task_delete(&conn, t.id).unwrap();
        assert!(!find("новое"), "deleted task must be gone");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn mixed_query_returns_file_and_task_hits_with_deterministic_ranking() {
        let (wd, conn, id, state) = setup("search-task-mixed");
        fs::write(root(&wd).join("redizayn.md"), "редизайн каталога товаров").unwrap();
        index::index_project(&state, id).unwrap();
        tasks::task_create(&conn, id, "редизайн главной страницы", None).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.kind == "file"));
        assert!(hits.iter().any(|h| h.kind == "task"));
        let again = search_project(&state, id, "редизайн", 10).unwrap();
        let keys = |hs: &[SearchHit]| -> Vec<(String, Option<i64>)> {
            hs.iter().map(|h| (h.kind.clone(), h.task_id)).collect()
        };
        assert_eq!(keys(&hits), keys(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_hits_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-task-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        tasks::task_create(&conn, other.id, "редизайн чужого проекта", None).unwrap();

        assert!(search_project(&state, id, "редизайн", 10).unwrap().is_empty());
        let hits = search_project(&state, other.id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "task");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_leg_adds_task_hit_without_literal_match() {
        let (wd, conn, id, _state) = setup("search-task-sem");
        tasks::task_create(&conn, id, "кошки", None).unwrap();

        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        let task = hits
            .iter()
            .find(|h| h.kind == "task")
            .expect("task semantic hit");
        assert_eq!(task.match_types, ["semantic"]);
        assert!(!task.snippet.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
}
