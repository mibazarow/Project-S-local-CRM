use crate::embed;
use crate::error::{AppError, Result};
use rusqlite::{params, Connection};
use std::cmp::Ordering;

use super::types::{task_row_from, TaskRow, TASK_COLS};

// Kanban-scale task counts: embed every task text of the project at query
// time instead of storing task vectors — always fresh, no sync or backfill
// burden, and the Noop default keeps this leg off entirely.
pub(crate) fn task_semantic_hits(
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

pub(crate) fn chat_semantic_hits(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, i64)>> {
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
    // Join back to messages/chats: vectors left by deleted messages or
    // sessions never surface as hits.
    let mut stmt = conn.prepare(
        "SELECT q.rowid, c.session_id
         FROM (SELECT rowid, distance FROM vec_messages WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance) q
         JOIN messages m ON m.id = q.rowid
         JOIN chats c ON c.id = m.chat_id AND c.kind = 'harness'
         WHERE c.session_id IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![blob, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
