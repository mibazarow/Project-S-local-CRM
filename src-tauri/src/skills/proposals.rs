// Repeat detector behind the skill proposals (SPEC 6.2 Q20): when the SAME
// complex task completes PROPOSAL_THRESHOLD times, the chat gets a one-time
// Russian proposal to save the workflow as a skill; the agent creates it only after the user
// confirms (the `create_skill` tool itself runs under the Check policy, so
// the standard confirmation dialog guards every write). No silent creation,
// no proposals for trivial runs, no repeated nagging: one row per signature,
// one proposal per signature across the whole app.
//
// A run is complex when it completed at least COMPLEX_TOOL_CALLS tool calls
// (finish_task excluded) or ran at least COMPLEX_MIN_SECS wall-clock. The
// signature of "the same task" is the normalized user request (lowercase,
// whitespace collapsed, cut to NORMALIZED_MAX_CHARS) plus the sorted unique
// set of tool names plus the completed tool-call count: the same workflow in
// a different call order is the same task, a materially different tool mix
// or volume is a different one.
use crate::chat;
use crate::error::Result;
use crate::llm::Role;
use crate::skills::manifest::find;
use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

pub const COMPLEX_TOOL_CALLS: usize = 4;
pub const COMPLEX_MIN_SECS: u64 = 120;
pub const PROPOSAL_THRESHOLD: i64 = 4;
const NORMALIZED_MAX_CHARS: usize = 200;
const TASK_QUOTE_MAX_CHARS: usize = 120;

// One completed run as the run path saw it.
pub struct RunSummary<'a> {
    pub chat_id: i64,
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
    pub user_text: &'a str,
    // Completed tool call names in call order; finish_task is filtered out
    // here, so callers can pass the raw journal list.
    pub tool_names: &'a [String],
    pub duration_secs: u64,
}

// The proposal landed in the chat: the persisted assistant message the UI
// shows live (and re-reads from history).
#[derive(Debug, Clone)]
pub struct Proposal {
    pub message_id: i64,
    pub text: String,
}

pub fn normalize_request(text: &str) -> String {
    let joined = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(NORMALIZED_MAX_CHARS).collect()
}

// Sorted unique tool names, finish_task excluded (every run calls it once,
// it carries no workflow information).
fn sorted_tools(tool_names: &[String]) -> Vec<String> {
    let mut tools: Vec<String> = tool_names
        .iter()
        .filter(|t| t.as_str() != "finish_task")
        .cloned()
        .collect();
    tools.sort();
    tools.dedup();
    tools
}

fn completed_tool_count(tool_names: &[String]) -> usize {
    tool_names.iter().filter(|t| t.as_str() != "finish_task").count()
}

fn hex_hash(parts: &[&str]) -> String {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

pub fn signature(normalized: &str, tools: &[String], tool_count: usize) -> String {
    hex_hash(&[normalized, &tools.join(","), &tool_count.to_string()])
}

// Skill-name candidate from the request: an ASCII slug of the normalized
// text (valid_skill_name shape in import.rs); Cyrillic requests have no
// ASCII slug, so they fall back to the signature prefix.
pub fn candidate_skill_name(normalized: &str, signature: &str) -> String {
    let slug: String = normalized
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let cut: String = slug.chars().take(48).collect();
    let cut = cut.trim_matches('-');
    let usable = !cut.is_empty()
        && cut.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && cut.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if usable {
        cut.to_string()
    } else {
        format!("task-{}", &signature[..8])
    }
}

fn proposal_text(name: &str, user_text: &str, tools: &[String]) -> String {
    let quote: String = user_text.trim().chars().take(TASK_QUOTE_MAX_CHARS).collect();
    format!(
        "Я заметил: одна и та же сложная задача выполнилась уже четвёртый раз (инструменты: {}). Предлагаю сохранить этот рабочий процесс как скилл «{}».\nЗадача: «{}»\nОтветь «да» — я создам скилл инструментом create_skill, создание потребует подтверждения в диалоге подтверждений. Ответь «нет» — и предложение больше не повторится.",
        tools.join(", "),
        name,
        quote
    )
}

// Counts one completed run against its signature and, when the signature
// reaches the threshold for the first time, appends the one-time proposal
// message to the run's chat. Returns the proposal when it was posted.
// Trivial runs never reach the table; once a signature is 'proposed' or
// 'skipped' it never proposes again (a declined or ignored proposal stays
// silent forever).
pub fn record_run(conn: &Connection, wd: &Path, run: &RunSummary<'_>) -> Result<Option<Proposal>> {
    let tools = sorted_tools(run.tool_names);
    let tool_count = completed_tool_count(run.tool_names);
    let complex = tool_count >= COMPLEX_TOOL_CALLS || run.duration_secs >= COMPLEX_MIN_SECS;
    let normalized = normalize_request(run.user_text);
    if !complex || normalized.is_empty() {
        return Ok(None);
    }
    let sig = signature(&normalized, &tools, tool_count);
    conn.execute(
        "INSERT INTO skill_proposals
           (signature, occurrences, status, last_text, tool_names, tool_count,
            chat_id, project_id, session_id, updated_at)
         VALUES (?1, 1, 'seen', ?2, ?3, ?4, ?5, ?6, ?7, datetime('now','localtime'))
         ON CONFLICT(signature) DO UPDATE SET
           occurrences = occurrences + 1,
           last_text = excluded.last_text,
           tool_names = excluded.tool_names,
           tool_count = excluded.tool_count,
           chat_id = excluded.chat_id,
           project_id = excluded.project_id,
           session_id = excluded.session_id,
           updated_at = excluded.updated_at",
        params![
            sig,
            run.user_text.trim(),
            serde_json::to_string(&tools)?,
            tool_count as i64,
            run.chat_id,
            run.project_id,
            run.session_id,
        ],
    )?;
    let (status, occurrences): (String, i64) = conn.query_row(
        "SELECT status, occurrences FROM skill_proposals WHERE signature = ?1",
        params![sig],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if status != "seen" || occurrences < PROPOSAL_THRESHOLD {
        return Ok(None);
    }
    // Do not duplicate an existing builtin/user skill.
    let name = candidate_skill_name(&normalized, &sig);
    if find(wd, &name).is_ok() {
        conn.execute(
            "UPDATE skill_proposals SET status = 'skipped' WHERE signature = ?1",
            params![sig],
        )?;
        return Ok(None);
    }
    let text = proposal_text(&name, run.user_text, &tools);
    let message = chat::append_message(conn, run.chat_id, Role::Assistant, Some(&text), &[], None, None)?;
    conn.execute(
        "UPDATE skill_proposals SET status = 'proposed' WHERE signature = ?1",
        params![sig],
    )?;
    Ok(Some(Proposal {
        message_id: message.id,
        text,
    }))
}

// Payload of the "agent-skill-proposal" event (same routing shape as the
// agent-file event): an open chat appends the proposal bubble live, keyed by
// the persisted message id so a later history reload never doubles it.
pub fn proposal_event_payload(
    chat_id: i64,
    project_id: Option<i64>,
    session_id: Option<i64>,
    notes: bool,
    proposal: &Proposal,
) -> serde_json::Value {
    let mut payload = json!({
        "chatId": chat_id,
        "messageId": proposal.message_id,
        "text": proposal.text,
    });
    if let Some(project_id) = project_id {
        payload["projectId"] = json!(project_id);
    }
    if let Some(session_id) = session_id {
        payload["sessionId"] = json!(session_id);
    }
    if notes {
        payload["notes"] = json!(true);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::paths;
    use crate::skills::testutil::{scaffold, write_skill};
    use rusqlite::Connection;
    use std::fs;

    fn setup(tag: &str) -> (std::path::PathBuf, Connection) {
        let wd = scaffold(tag);
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('harness', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        (wd, conn)
    }

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn run<'a>(user_text: &'a str, tool_names: &'a [String], duration_secs: u64) -> RunSummary<'a> {
        RunSummary {
            chat_id: 1,
            project_id: None,
            session_id: None,
            user_text,
            tool_names,
            duration_secs,
        }
    }

    fn proposal_messages(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT content FROM messages WHERE role = 'assistant' ORDER BY position")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn fourth_complex_repeat_posts_exactly_one_proposal() {
        let (wd, conn) = setup("proposals-four-times");
        let t = tools(&["webfetch", "websearch", "write_file", "bash", "finish_task"]);
        for _ in 0..3 {
            assert!(
                record_run(&conn, &wd, &run("Сводка по example.ru", &t, 10)).unwrap().is_none(),
                "occurrences below the threshold must not propose"
            );
        }
        let fourth = record_run(&conn, &wd, &run("сводка по example.ru", &t, 10))
            .unwrap()
            .expect("the fourth occurrence proposes");
        assert!(fourth.text.contains("четвёртый раз"), "{}", fourth.text);
        assert!(fourth.text.contains("webfetch"), "{}", fourth.text);
        let stored = proposal_messages(&conn);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0], fourth.text);
        // Fifth and sixth repeats: never a second proposal (decline/ignore
        // both mean silence).
        assert!(record_run(&conn, &wd, &run("Сводка по example.ru", &t, 10)).unwrap().is_none());
        assert!(record_run(&conn, &wd, &run("Сводка по example.ru", &t, 10)).unwrap().is_none());
        assert_eq!(proposal_messages(&conn).len(), 1);
        let occurrences: i64 = conn
            .query_row("SELECT occurrences FROM skill_proposals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(occurrences, 6);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trivial_runs_are_not_counted() {
        let (wd, conn) = setup("proposals-trivial");
        let t = tools(&["webfetch", "finish_task"]);
        for _ in 0..5 {
            assert!(record_run(&conn, &wd, &run("какой сегодня день", &t, 5)).unwrap().is_none());
        }
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM skill_proposals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "trivial runs must not reach the table");
        assert!(proposal_messages(&conn).is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn long_duration_makes_a_few_tool_calls_complex() {
        let (wd, conn) = setup("proposals-duration");
        let t = tools(&["webfetch", "finish_task"]);
        for _ in 0..3 {
            assert!(record_run(&conn, &wd, &run("должая задача", &t, COMPLEX_MIN_SECS)).unwrap().is_none());
        }
        assert!(record_run(&conn, &wd, &run("должая задача", &t, COMPLEX_MIN_SECS)).unwrap().is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn different_tasks_do_not_share_a_signature() {
        let (wd, conn) = setup("proposals-distinct");
        let t = tools(&["webfetch", "websearch", "write_file", "bash", "finish_task"]);
        let other = tools(&["webfetch", "websearch", "bash", "ocr", "finish_task"]);
        let reordered = tools(&["bash", "write_file", "websearch", "webfetch", "finish_task"]);
        assert!(record_run(&conn, &wd, &run("сводка по ценам", &t, 10)).unwrap().is_none());
        assert!(record_run(&conn, &wd, &run("сверка актов", &t, 10)).unwrap().is_none());
        assert!(proposal_messages(&conn).is_empty());
        // Same request, different tool mix: a different workflow.
        assert!(record_run(&conn, &wd, &run("сводка по ценам", &other, 10)).unwrap().is_none());
        assert!(proposal_messages(&conn).is_empty());
        // Same request + same tools in a different call order: the same task,
        // so the variants stack onto one signature until the threshold (4).
        assert!(record_run(&conn, &wd, &run("СВОДКА   по   ценам", &reordered, 10)).unwrap().is_none());
        assert!(record_run(&conn, &wd, &run("сводка по ценам", &t, 10)).unwrap().is_none());
        let same = record_run(&conn, &wd, &run("СВОДКА по ценам", &reordered, 10)).unwrap();
        assert!(same.is_some(), "case/whitespace/call-order variants must share the signature");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn existing_skill_candidate_suppresses_the_proposal() {
        let (wd, conn) = setup("proposals-existing");
        let text = "сводка по ценам";
        let t = tools(&["webfetch", "websearch", "write_file", "bash", "finish_task"]);
        // The candidate name the detector will pick for this run, pre-created
        // as a builtin: the proposal must be skipped, not shown.
        let normalized = normalize_request(text);
        let sig = signature(&normalized, &sorted_tools(&t), completed_tool_count(&t));
        let name = candidate_skill_name(&normalized, &sig);
        assert!(name.starts_with("task-"), "{name}");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            &name,
            &format!("---\nname: {name}\ndescription: d\n---\nB.\n"),
        );
        for _ in 0..4 {
            assert!(record_run(&conn, &wd, &run(text, &t, 10)).unwrap().is_none());
        }
        assert!(proposal_messages(&conn).is_empty());
        let status: String = conn
            .query_row("SELECT status FROM skill_proposals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "skipped");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn candidate_skill_name_slugs_ascii_and_falls_back_for_cyrillic() {
        assert_eq!(candidate_skill_name("daily price report", "abcdef0123456789"), "daily-price-report");
        assert_eq!(candidate_skill_name("2026 отчёт", "abcdef0123456789"), "2026");
        assert_eq!(candidate_skill_name("сводка по ценам", "abcdef0123456789"), "task-abcdef01");
        let long = "a".repeat(80);
        let cut = candidate_skill_name(&long, "abcdef0123456789");
        assert_eq!(cut.len(), 48);
    }

    #[test]
    fn proposal_event_payload_routes_by_run_key() {
        let proposal = Proposal {
            message_id: 7,
            text: "тест".into(),
        };
        let project = proposal_event_payload(3, Some(9), None, false, &proposal);
        assert_eq!(project["chatId"], 3);
        assert_eq!(project["projectId"], 9);
        assert_eq!(project["messageId"], 7);
        assert!(project.get("sessionId").is_none());
        assert!(project.get("notes").is_none());
        let notes = proposal_event_payload(4, None, None, true, &proposal);
        assert_eq!(notes["notes"], true);
        assert!(notes.get("projectId").is_none());
    }
}
