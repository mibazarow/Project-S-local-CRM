// Cached project brief: a short Russian summary of what a project is about,
// generated from heuristic material (name, top-level tree, datas.md excerpt,
// task summary, key file names) plus excerpts of up to a few supported
// documents, condensed by the LLM when it is reachable and falling back to
// the heuristic otherwise. The result is cached in projects.brief /
// projects.brief_updated_at, refreshed when stale (project open, watcher
// reindex, or the project_brief command) and injected into the project
// system prompt. Secrets from datas.md and documents are masked before they
// ever reach the model or the cache.
use crate::agent::r#loop::ChatBackend;
use crate::config::{self, ConfigKind, LlmConfig};
use crate::db;
use crate::error::{AppError, Result};
use crate::extract;
use crate::llm::{self, ChatMessage, ChatParams};
use crate::paths;
use crate::projects;
use crate::state::AppState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

// A brief older than this is regenerated on the next ensure (project open,
// watcher reindex, or the project_brief command).
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

const HEURISTIC_CAP: usize = 1500;
const BRIEF_CAP: usize = 2000;
const DOC_LIMIT: usize = 6;
const DOC_EXCERPT_CHARS: usize = 400;
const DATAS_EXCERPT_CHARS: usize = 300;
const TOP_LEVEL_LIMIT: usize = 40;
const RECENT_TASK_LIMIT: usize = 5;
const WALK_VISIT_CAP: usize = 500;

// Case-insensitive key markers: a "key: value" / "key = value" line whose key
// contains one of these has its value replaced with ***. Deliberately narrow
// (no bare "ключ"/"pass") so ordinary datas.md lines survive untouched.
const SECRET_KEY_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "пароль",
    "api_key",
    "apikey",
    "api-key",
    "token",
    "secret",
    "секрет",
    "access_key",
    "private_key",
    "ключ доступа",
    "секретный ключ",
    "приватный ключ",
];

const BRIEF_SYSTEM_PROMPT: &str = "You are writing a project brief for an AI agent that works inside this project. \
Below is project material: a heuristic summary (project name, top-level tree, datas.md excerpt, task summary, \
key files) followed by excerpts of key documents. Secrets in the material are already masked.\n\
Write a concise brief IN RUSSIAN, 1000-2000 characters, plain text. Cover:\n\
- what the project is about and what its current task is;\n\
- the key documents and where they live;\n\
- the current state of work (tasks, deadlines, overdue items);\n\
- important access data and constraints from datas.md — refer to them as \"см. datas.md\", \
never reproduce passwords, API keys or tokens.\n\
Output only the brief text, no preamble and no code fences.";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefDto {
    pub text: String,
    pub updated_at: String,
}

// Payload for the "project-brief-changed" UI event.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefChanged {
    pub project_id: i64,
}

// Masks obvious key/value secrets (passwords, API keys, tokens) line by line,
// keeping the key and hiding the value.
pub(crate) fn mask_secrets(text: &str) -> String {
    text.lines()
        .map(|line| {
            if let Some(pos) = line.find([':', '=']) {
                let key = line[..pos].trim().to_lowercase();
                if SECRET_KEY_MARKERS.iter().any(|m| key.contains(m)) {
                    return format!("{} ***", line[..pos].trim_end());
                }
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Deterministic fallback brief, capped at ~1500 chars: project name, bounded
// top-level tree, datas.md excerpt (masked), task counts/overdue/recent
// titles, and a few key document names.
pub fn heuristic_brief(conn: &Connection, wd: &Path, project_id: i64) -> Result<String> {
    let project = projects::get_project(conn, project_id)?;
    let root = projects::project_root(conn, wd, project_id)?;
    let mut out = format!("Project \"{}\"\n", project.name);

    let top = top_level(&root, TOP_LEVEL_LIMIT);
    if !top.is_empty() {
        out.push_str(&format!("Top level: {}\n", top.join(", ")));
    }
    let docs = supported_documents(&root, DOC_LIMIT);
    if !docs.is_empty() {
        let names: Vec<&str> = docs.iter().map(|(rel, _)| rel.as_str()).collect();
        out.push_str(&format!("Key files: {}\n", names.join(", ")));
    }
    if let Ok(text) = fs::read_to_string(root.join(paths::DATAS_MD)) {
        let excerpt = extract::truncate_chars(&mask_secrets(&text), DATAS_EXCERPT_CHARS);
        let excerpt = excerpt.trim();
        if !excerpt.is_empty() {
            out.push_str(&format!("datas.md excerpt: {}\n", excerpt.replace('\n', " | ")));
        }
    }

    let total: i64 = conn.query_row(
        "SELECT count(*) FROM tasks WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    if total > 0 {
        let counts: Vec<(String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT status, count(*) FROM tasks WHERE project_id = ?1 GROUP BY status",
            )?;
            let rows = stmt.query_map(params![project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.flatten().collect()
        };
        let parts: Vec<String> = crate::tasks::STATUSES
            .iter()
            .filter_map(|status| {
                counts
                    .iter()
                    .find(|(s, _)| s == status)
                    .map(|(_, n)| format!("{status}={n}"))
            })
            .collect();
        let overdue: i64 = conn.query_row(
            "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status != 'done' \
             AND due_date IS NOT NULL AND due_date || ' ' || due_time || ':00' < datetime('now','localtime')",
            params![project_id],
            |r| r.get(0),
        )?;
        out.push_str(&format!(
            "Tasks: total={total} ({}), overdue={overdue}\n",
            parts.join(", ")
        ));
        let recent: Vec<String> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT title, status, COALESCE(due_date, '') FROM tasks WHERE project_id = ?1 \
                 ORDER BY created_at DESC, id DESC LIMIT {RECENT_TASK_LIMIT}"
            ))?;
            let rows = stmt.query_map(params![project_id], |r| {
                let title: String = r.get(0)?;
                let status: String = r.get(1)?;
                let due: String = r.get(2)?;
                let due = if due.is_empty() {
                    String::new()
                } else {
                    format!(", due {due}")
                };
                Ok(format!(
                    "\"{}\" [{status}{due}]",
                    title.replace(['\n', '\r'], " ")
                ))
            })?;
            rows.flatten().collect()
        };
        if !recent.is_empty() {
            out.push_str(&format!("Recent tasks: {}\n", recent.join("; ")));
        }
    } else {
        out.push_str("Tasks: none\n");
    }

    Ok(extract::truncate_chars(out.trim(), HEURISTIC_CAP))
}

// Full generation: heuristic material + document excerpts through the LLM
// (English instructions, Russian brief out); any LLM error or empty answer
// falls back to the heuristic. Never raises: the brief is best-effort.
pub fn generate_brief(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    backend: &dyn ChatBackend,
) -> String {
    let heuristic = heuristic_brief(conn, wd, project_id).unwrap_or_default();
    let material = gather_material(conn, wd, project_id, &heuristic);
    let params = brief_params(wd);
    let messages = vec![
        ChatMessage::system(BRIEF_SYSTEM_PROMPT),
        ChatMessage::user(material),
    ];
    let text = match backend.chat_stream(&messages, None, &params, &mut |_| {}) {
        Ok(outcome) => outcome.content.unwrap_or_default(),
        Err(_) => String::new(),
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        heuristic
    } else {
        extract::truncate_chars(&text, BRIEF_CAP)
    }
}

fn gather_material(conn: &Connection, wd: &Path, project_id: i64, heuristic: &str) -> String {
    let mut material = format!("=== Project material (secrets masked) ===\n{heuristic}");
    if let Ok(root) = projects::project_root(conn, wd, project_id) {
        for (rel, path) in supported_documents(&root, DOC_LIMIT) {
            if let Ok(text) = extract::extract_text(&path, DOC_EXCERPT_CHARS) {
                material.push_str(&format!("\n\n--- {rel} ---\n{}", mask_secrets(&text)));
            }
        }
    }
    material
}

fn brief_params(wd: &Path) -> ChatParams {
    let cfg: LlmConfig = config::read_config(wd, ConfigKind::Llm)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    ChatParams::from(&cfg)
}

// Bounded walk of the project folder collecting supported documents (the
// root datas.md is skipped: its excerpt is already in the heuristic), sorted
// by rel path for determinism.
fn supported_documents(root: &Path, limit: usize) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for item in entries.flatten() {
            visited += 1;
            if visited > WALK_VISIT_CAP {
                break;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = item.path();
            if path.is_dir() {
                stack.push(path);
            } else if name != paths::DATAS_MD && extract::is_supported(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    found.push((rel.to_string_lossy().into_owned(), path));
                }
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.truncate(limit);
    found
}

fn top_level(root: &Path, limit: usize) -> Vec<String> {
    let mut entries: Vec<String> = fs::read_dir(root)
        .map(|items| {
            items
                .flatten()
                .filter_map(|item| {
                    let name = item.file_name().to_string_lossy().into_owned();
                    (!name.starts_with('.')).then(|| {
                        if item.path().is_dir() {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|n| n.to_lowercase());
    entries.truncate(limit);
    entries
}

// Cached brief; None when the project has none yet (or does not exist).
pub fn get_brief(conn: &Connection, project_id: i64) -> Result<Option<BriefDto>> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT brief, brief_updated_at FROM projects WHERE id = ?1",
            params![project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(text, updated_at)| match (text, updated_at) {
        (Some(text), Some(updated_at)) => Some(BriefDto { text, updated_at }),
        _ => None,
    }))
}

pub fn store_brief(conn: &Connection, project_id: i64, text: &str) -> Result<BriefDto> {
    let updated = conn.execute(
        "UPDATE projects SET brief = ?1, brief_updated_at = datetime('now','localtime') WHERE id = ?2",
        params![text, project_id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {project_id} not found")));
    }
    get_brief(conn, project_id)?
        .ok_or_else(|| AppError::Db(format!("brief for project {project_id} vanished")))
}

fn brief_is_fresh(conn: &Connection, project_id: i64, max_age: Duration) -> Result<bool> {
    let minutes = max_age.as_secs() / 60;
    let fresh: i64 = conn.query_row(
        "SELECT count(*) FROM projects
         WHERE id = ?1 AND brief IS NOT NULL
           AND brief_updated_at >= datetime('now', 'localtime', ?2)",
        params![project_id, format!("-{minutes} minutes")],
        |r| r.get(0),
    )?;
    Ok(fresh == 1)
}

// Generate now (through the configured LLM) and cache the result.
pub fn refresh_brief(conn: &Connection, wd: &Path, project_id: i64) -> Result<BriefDto> {
    let client = llm::client_from_config(wd)?;
    refresh_brief_with(conn, wd, project_id, &client)
}

pub fn refresh_brief_with(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    backend: &dyn ChatBackend,
) -> Result<BriefDto> {
    let text = generate_brief(conn, wd, project_id, backend);
    store_brief(conn, project_id, &text)
}

// Cached-or-generate: returns the brief plus whether it was regenerated.
pub fn ensure_brief_on(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    max_age: Duration,
) -> Result<(BriefDto, bool)> {
    if brief_is_fresh(conn, project_id, max_age)? {
        let dto = get_brief(conn, project_id)?
            .ok_or_else(|| AppError::Invalid(format!("project {project_id} not found")))?;
        return Ok((dto, false));
    }
    refresh_brief(conn, wd, project_id).map(|dto| (dto, true))
}

// Cached-or-generate with an injected backend (the test seam; production
// paths go through ensure_brief_on / refresh_brief with the configured LLM).
#[cfg(test)]
pub fn ensure_brief_with(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    max_age: Duration,
    backend: &dyn ChatBackend,
) -> Result<(BriefDto, bool)> {
    if brief_is_fresh(conn, project_id, max_age)? {
        let dto = get_brief(conn, project_id)?
            .ok_or_else(|| AppError::Invalid(format!("project {project_id} not found")))?;
        return Ok((dto, false));
    }
    refresh_brief_with(conn, wd, project_id, backend).map(|dto| (dto, true))
}

// AppState variant for background triggers: opens its own connection (same
// WAL pattern as index_project) so it never blocks the UI connection.
pub fn ensure_brief(
    state: &AppState,
    project_id: i64,
    max_age: Duration,
) -> Result<(BriefDto, bool)> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    ensure_brief_on(&conn, &wd, project_id, max_age)
}

// Background trigger used on project open and after a watcher reindex: one
// cheap freshness check when the brief is fresh, a full regeneration only
// when it is missing or stale. Emits "project-brief-changed" on regeneration.
pub fn schedule_refresh(app: &tauri::AppHandle, project_id: i64) {
    let app = app.clone();
    std::thread::spawn(move || {
        use tauri::{Emitter, Manager};
        let state = app.state::<AppState>();
        if let Ok((_, generated)) = ensure_brief(state.inner(), project_id, MAX_AGE) {
            if generated {
                let _ = app.emit("project-brief-changed", BriefChanged { project_id });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::llm::{ChatEvent, ChatOutcome, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FakeBackend {
        reply: Option<String>,
        fail: bool,
        calls: AtomicUsize,
        last_user: Mutex<Option<String>>,
    }

    impl FakeBackend {
        fn reply(text: &str) -> Self {
            FakeBackend {
                reply: Some(text.into()),
                fail: false,
                calls: AtomicUsize::new(0),
                last_user: Mutex::new(None),
            }
        }

        fn failing() -> Self {
            FakeBackend {
                reply: None,
                fail: true,
                calls: AtomicUsize::new(0),
                last_user: Mutex::new(None),
            }
        }
    }

    impl ChatBackend for FakeBackend {
        fn chat_stream(
            &self,
            messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(text) = messages.last().and_then(|m| m.text_content()) {
                *self.last_user.lock().unwrap() = Some(text.to_string());
            }
            if self.fail {
                return Err(AppError::Http("llm unavailable".into()));
            }
            Ok(ChatOutcome {
                content: self.reply.clone(),
                reasoning: None,
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            })
        }
    }

    fn setup(tag: &str) -> (PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let dto = projects::create_project(&conn, &wd, "example.ru").unwrap();
        (wd, conn, dto.id)
    }

    fn seed_project(wd: &Path) -> PathBuf {
        let root = paths::projects_dir(wd).join("example.ru");
        fs::write(
            root.join(paths::DATAS_MD),
            "Доступы к example.ru\nFTP: ftp.example.ru\npassword: hunter2\napi_key = sk-abc123\n",
        )
        .unwrap();
        fs::write(root.join("spec.md"), "# ТЗ\nСделать лендинг для example.ru").unwrap();
        fs::create_dir_all(root.join("Работа")).unwrap();
        fs::write(root.join("Работа/акт.txt"), "акт выполненных работ").unwrap();
        root
    }

    #[test]
    fn mask_secrets_masks_key_value_lines_only() {
        let text = "ftp: ftp.example.ru\npassword: hunter2\nПароль = секрет123\n\
                    api_key: sk-abc\nlogin: user1\nключевые слова: важно";
        let masked = mask_secrets(text);
        assert!(masked.contains("ftp.example.ru"));
        assert!(masked.contains("login: user1"));
        assert!(masked.contains("ключевые слова: важно"));
        assert!(!masked.contains("hunter2"));
        assert!(!masked.contains("секрет123"));
        assert!(!masked.contains("sk-abc"));
        assert!(masked.contains("password ***"));
    }

    #[test]
    fn heuristic_brief_covers_tree_datas_tasks_and_masks_secrets() {
        let (wd, conn, pid) = setup("brief-heuristic");
        seed_project(&wd);
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, due_date, created_at) VALUES
             (?1, 'сделать лендинг', 'new', date('now','+3 day'), '2026-01-01'),
             (?1, 'согласовать макет', 'progress', date('now','-2 day'), '2026-01-02'),
             (?1, 'домен оплачен', 'done', NULL, '2026-01-03')",
            params![pid],
        )
        .unwrap();
        let brief = heuristic_brief(&conn, &wd, pid).unwrap();
        assert!(brief.contains("example.ru"), "{brief}");
        assert!(brief.contains("spec.md"), "{brief}");
        assert!(brief.contains("Работа/"), "{brief}");
        assert!(brief.contains("ftp.example.ru"), "{brief}");
        assert!(brief.contains("Tasks: total=3"), "{brief}");
        assert!(brief.contains("new=1"), "{brief}");
        assert!(brief.contains("progress=1"), "{brief}");
        assert!(brief.contains("done=1"), "{brief}");
        assert!(brief.contains("overdue=1"), "{brief}");
        assert!(brief.contains("согласовать макет"), "{brief}");
        assert!(!brief.contains("hunter2"), "secret leaked into heuristic");
        assert!(!brief.contains("sk-abc123"), "secret leaked into heuristic");
        assert!(brief.chars().count() <= HEURISTIC_CAP, "len {}", brief.chars().count());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn generate_brief_falls_back_to_heuristic_when_llm_fails() {
        let (wd, conn, pid) = setup("brief-fallback");
        seed_project(&wd);
        let heuristic = heuristic_brief(&conn, &wd, pid).unwrap();
        let backend = FakeBackend::failing();
        let brief = generate_brief(&conn, &wd, pid, &backend);
        assert_eq!(brief, heuristic);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn generate_brief_uses_llm_and_never_sends_secrets() {
        let (wd, conn, pid) = setup("brief-llm");
        seed_project(&wd);
        let backend = FakeBackend::reply("Краткое описание: проект example.ru — лендинг под ключ.");
        let brief = generate_brief(&conn, &wd, pid, &backend);
        assert_eq!(brief, "Краткое описание: проект example.ru — лендинг под ключ.");
        let material = backend.last_user.lock().unwrap().clone().unwrap();
        assert!(material.contains("Project material"), "{material}");
        assert!(material.contains("example.ru"));
        assert!(material.contains("spec.md"));
        assert!(!material.contains("hunter2"), "secret reached the model");
        assert!(!material.contains("sk-abc123"), "secret reached the model");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn ensure_brief_caches_within_max_age_and_regenerates_when_stale() {
        let (wd, conn, pid) = setup("brief-cache");
        seed_project(&wd);
        let backend = FakeBackend::reply("бриф v1");
        let (dto, generated) = ensure_brief_with(&conn, &wd, pid, MAX_AGE, &backend).unwrap();
        assert!(generated);
        assert_eq!(dto.text, "бриф v1");
        assert!(dto.updated_at.len() >= 10);

        let (dto2, generated2) = ensure_brief_with(&conn, &wd, pid, MAX_AGE, &backend).unwrap();
        assert!(!generated2, "regenerated within max_age");
        assert_eq!(dto2.text, "бриф v1");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);

        conn.execute(
            "UPDATE projects SET brief_updated_at = '2000-01-01 00:00:00' WHERE id = ?1",
            params![pid],
        )
        .unwrap();
        let backend2 = FakeBackend::reply("бриф v2");
        let (dto3, generated3) = ensure_brief_with(&conn, &wd, pid, MAX_AGE, &backend2).unwrap();
        assert!(generated3, "stale brief not regenerated");
        assert_eq!(dto3.text, "бриф v2");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn store_and_get_brief_round_trip_and_unknown_project() {
        let (wd, conn, pid) = setup("brief-store");
        assert!(get_brief(&conn, pid).unwrap().is_none());
        let dto = store_brief(&conn, pid, "текст брифа").unwrap();
        assert_eq!(dto.text, "текст брифа");
        assert_eq!(get_brief(&conn, pid).unwrap().unwrap().text, "текст брифа");
        assert!(matches!(
            store_brief(&conn, 999, "x"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }
}
