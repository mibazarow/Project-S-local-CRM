// Harness session self-management (chat tools): the agent sees its own
// session list and can rename sessions. rename_chat defaults to the session
// the run belongs to (ToolCtx session_id) and emits "sessions-changed" so
// the sidebar follows the new title without a manual reload. Agent-side chat
// management: new_chat opens a separate session (its own context, the global
// memory stays shared) and delete_chat removes one, including the session
// the run itself lives in (the run then finishes into a deleted chat:
// transcript appends fail FK-safe and are dropped, never a crash). Chat
// management is safe, so both stay Mutating/Skip; every success emits
// "sessions-changed" so the sidebar follows immediately.
use crate::agent::project_tools::{envelope, state_of, with_conn};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::envelope_r;
use crate::error::Result;
use crate::sync_util::lock_recover;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

pub struct ListChatsTool;

impl Tool for ListChatsTool {
    fn name(&self) -> &str {
        "list_chats"
    }
    fn description(&self) -> &str {
        "List harness sessions (id, title, lastActive), most recently active first."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let r = with_conn(ctx, crate::harness::list_sessions);
        Ok(match r {
            Ok(list) => {
                let chats: Vec<Value> = list
                    .into_iter()
                    .map(|s| json!({ "id": s.id, "title": s.title, "lastActive": s.last_active_at }))
                    .collect();
                ToolResult::ok(json!({ "chats": chats })).capped(self.max_result_len())
            }
            Err(e) => envelope_r(e),
        })
    }
}

const RENAME_CHAT_MAX_TITLE_CHARS: usize = 120;

pub struct RenameChatTool;

impl Tool for RenameChatTool {
    fn name(&self) -> &str {
        "rename_chat"
    }
    fn description(&self) -> &str {
        "Rename a harness session: sessionId picks it explicitly, absent means the session this run belongs to. Title is a short single-line name."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "number" },
                "title": { "type": "string" }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let title = args["title"].as_str().unwrap_or_default().trim().to_owned();
        if title.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title is empty"));
        }
        if title.contains('\n') {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title must be a single line"));
        }
        if title.chars().count() > RENAME_CHAT_MAX_TITLE_CHARS {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("title is over {RENAME_CHAT_MAX_TITLE_CHARS} characters"),
            ));
        }
        let session_id = match args.get("sessionId").and_then(Value::as_i64) {
            Some(id) => id,
            None => match ctx.session_id {
                Some(id) => id,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId is required outside a session run",
                    ))
                }
            },
        };
        let r = with_conn(ctx, |conn| crate::harness::rename_session(conn, session_id, &title));
        if r.is_ok() {
            emit_sessions_changed(ctx);
        }
        Ok(envelope(r))
    }
}

// "sessions-changed" through the run state's emitter (same seam as rename_chat
// and todowrite): the sidebar follows session create/delete without a manual
// reload; a missing emitter is not an error.
pub(crate) fn emit_sessions_changed(ctx: &ToolCtx) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("sessions-changed", json!({}));
        }
    }
}

pub struct NewChatTool;

const NEW_CHAT_DEFAULT_TITLE: &str = "Новый чат";

impl Tool for NewChatTool {
    fn name(&self) -> &str {
        "new_chat"
    }
    fn description(&self) -> &str {
        "Create a new harness session: a separate chat with its own context (only the global memory is shared). Optional short single-line title, default «Новый чат». Returns the new sessionId and title."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let title = match args.get("title").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => {
                let title = raw.trim();
                if title.contains('\n') {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "title must be a single line",
                    ));
                }
                if title.chars().count() > RENAME_CHAT_MAX_TITLE_CHARS {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        format!("title is over {RENAME_CHAT_MAX_TITLE_CHARS} characters"),
                    ));
                }
                title.to_owned()
            }
            _ => NEW_CHAT_DEFAULT_TITLE.to_owned(),
        };
        let r = with_conn(ctx, |conn| crate::harness::create_session(conn, Some(&title)));
        if r.is_ok() {
            emit_sessions_changed(ctx);
        }
        Ok(match r {
            Ok(s) => ToolResult::ok(json!({ "sessionId": s.id, "title": s.title })),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DeleteChatTool;

impl Tool for DeleteChatTool {
    fn name(&self) -> &str {
        "delete_chat"
    }
    fn description(&self) -> &str {
        "Delete a harness session and its messages: sessionId picks it explicitly, absent means the session this run belongs to (deleting the current chat is allowed; the run then finishes without persisting more). Schedules of the deleted session survive and fire into a fresh session."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "number" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        // Deleting a session and all its messages is destructive and
        // irreversible from the agent's side, so it confirms like every other
        // delete in the catalog (fs_delete, task_delete, crm_task_delete).
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let session_id = match args.get("sessionId") {
            Some(value) => match value.as_i64() {
                Some(id) if id > 0 => id,
                _ => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId must be a positive integer",
                    ))
                }
            },
            None => match ctx.session_id {
                Some(id) => id,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId is required outside a session run",
                    ))
                }
            },
        };
        let current = ctx.session_id == Some(session_id);
        let r = with_conn(ctx, |conn| crate::harness::delete_session(conn, session_id));
        if r.is_ok() {
            emit_sessions_changed(ctx);
            // delete_session cascades to the chat's schedules: the schedules
            // panel must drop them too, not leave them orphaned on screen.
            if let Ok(state) = state_of(ctx) {
                if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
                    emitter.emit_event("schedules-changed", json!({}));
                }
            }
        }
        Ok(match r {
            Ok(()) => ToolResult::ok(json!({ "deleted": true, "current": current })),
            Err(e) => envelope_r(e),
        })
    }
}

// Hybrid search over past harness sessions (FTS + vectors, schema v19):
// one hit per session with the best-matching message and its snippet.
pub struct SearchChatsTool;

impl Tool for SearchChatsTool {
    fn name(&self) -> &str {
        "search_chats"
    }
    fn description(&self) -> &str {
        "Find harness chats by meaning (keyword + semantic), most relevant first. Each result is a chat: sessionId, title, digest (its latest summary or first user message), score, plus a matched-message snippet. Report each candidate with its title and digest so the user can tell them apart; when several match, offer the most relevant first. Use open_chat to switch the UI to one when the user asks."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number", "minimum": 1, "maximum": 50 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let query = args["query"].as_str().unwrap_or_default().trim().to_owned();
        if query.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "query is empty"));
        }
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let state = match state_of(ctx) {
            Ok(state) => state,
            Err(e) => return Ok(envelope_r(e)),
        };
        let r = crate::search::search_chats(state, &query, limit);
        Ok(match r {
            Ok(hits) => ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

// Open a chat the agent found by meaning (search_chats) when the user asks to
// open it: emits "chat-open" {sessionId} through the run state's emitter (the
// same seam the schedule-open navigation uses), so App switches to the Harness
// tab and HarnessView loads that session. UI navigation only — no data change,
// no confirmation — so it stays Mutating/Skip like the open/notify tools.
pub struct OpenChatTool;

impl Tool for OpenChatTool {
    fn name(&self) -> &str {
        "open_chat"
    }
    fn description(&self) -> &str {
        "Open a harness chat in the UI by sessionId (from search_chats or list_chats): the app switches to that session's chat. UI navigation only, no data changes. Returns {opened, title}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "number" }
            },
            "required": ["sessionId"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let session_id = match args.get("sessionId").and_then(Value::as_i64) {
            Some(id) if id > 0 => id,
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "sessionId must be a positive integer",
                ))
            }
        };
        let r = with_conn(ctx, |conn| {
            crate::harness::session_chat_id(conn, session_id)?;
            let title = conn
                .query_row(
                    "SELECT title FROM sessions WHERE id = ?1",
                    params![session_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            Ok(title)
        });
        match r {
            Ok(title) => {
                if let Ok(state) = state_of(ctx) {
                    if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
                        emitter.emit_event("chat-open", json!({ "sessionId": session_id }));
                    }
                }
                Ok(ToolResult::ok(json!({ "opened": session_id, "title": title })))
            }
            Err(e) => Ok(envelope_r(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup};
    use crate::error::AppError;
    use crate::state::AppState;
    use serde_json::json;
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn search_chats_tool_finds_harness_messages_end_to_end() {
        let env = setup("ht-search-chats");
        let state = env.ctx.state.clone().unwrap();
        {
            let conn = state.db.lock().unwrap();
            let conn = conn.as_ref().unwrap();
            let session = crate::harness::create_session(conn, Some("витрина")).unwrap();
            let chat_id = crate::harness::session_chat_id(conn, session.id).unwrap();
            crate::chat::append_message(
                conn,
                chat_id,
                crate::llm::Role::User,
                Some("нужен редизайн главной страницы"),
                &[],
                None,
                None,
            )
            .unwrap();
        }

        let res = exec("search_chats", json!({ "query": "редизайн" }), &env.ctx);
        assert!(res.ok, "{:?}", res.error);
        let hits = res.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["sessionId"], session_id_of(&state));
        assert_eq!(hits[0]["title"], "витрина");
        assert!(hits[0]["snippet"].as_str().unwrap().contains("<<редизайн>>"));
        assert_eq!(hits[0]["digest"], "нужен редизайн главной страницы");

        let empty = exec("search_chats", json!({ "query": "  " }), &env.ctx);
        assert!(!empty.ok);
        assert_eq!(empty.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    fn session_id_of(state: &Arc<AppState>) -> i64 {
        let conn = state.db.lock().unwrap();
        conn.as_ref()
            .unwrap()
            .query_row("SELECT id FROM sessions LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn open_chat_tool_emits_chat_open_and_validates() {
        let env = setup("ht-open-chat");
        let created =
            with_conn(&env.ctx, |conn| crate::harness::create_session(conn, Some("витрина")))
                .unwrap();

        let r = exec("open_chat", json!({ "sessionId": created.id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let out = r.data.unwrap();
        assert_eq!(out["opened"], created.id);
        assert_eq!(out["title"], "витрина");

        // The UI navigation event carries the sessionId through the emitter seam.
        let events = env.events.lock().unwrap().clone();
        let opened = events
            .iter()
            .filter(|(name, payload)| name == "chat-open" && payload["sessionId"] == created.id)
            .count();
        assert_eq!(opened, 1, "chat-open must fire once with the sessionId: {events:?}");

        // Unknown session maps to not_found; non-positive/non-integer/missing
        // sessionId are argument errors, and none of them emit an event.
        let r = exec("open_chat", json!({ "sessionId": 999 }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        for bad in [
            json!({ "sessionId": 0 }),
            json!({ "sessionId": -3 }),
            json!({ "sessionId": "x" }),
            json!({}),
        ] {
            let r = exec("open_chat", bad, &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }
        let events = env.events.lock().unwrap().clone();
        assert_eq!(
            events.iter().filter(|(name, _)| name == "chat-open").count(),
            1,
            "only the successful open emits: {events:?}"
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn list_and_rename_chat_tools_round_trip() {
        let env = setup("ht-chats");
        let a = with_conn(&env.ctx, |conn| crate::harness::create_session(conn, Some("a"))).unwrap();
        let b = with_conn(&env.ctx, |conn| crate::harness::create_session(conn, None)).unwrap();

        let r = exec("list_chats", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let chats = r.data.unwrap()["chats"].as_array().unwrap().clone();
        assert_eq!(chats.len(), 2);
        // Same-second creation ties on last_active_at: the id DESC tiebreak
        // puts the newer session first, matching the UI list order.
        assert_eq!(chats[0]["id"], b.id);
        assert_eq!(chats[0]["title"], Value::Null);
        assert!(chats[0]["lastActive"].is_string());
        assert_eq!(chats[1]["id"], a.id);
        assert_eq!(chats[1]["title"], "a");

        // Explicit sessionId renames that session.
        let r = exec("rename_chat", json!({ "sessionId": a.id, "title": " Переименована " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["title"], "Переименована");

        // No sessionId: the ToolCtx session of the run is renamed.
        let ctx = ToolCtx {
            session_id: Some(b.id),
            ..env.ctx
        };
        let r = exec("rename_chat", json!({ "title": "Текущая" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["title"], "Текущая");

        // Empty / multiline / overlong titles are rejected before touching DB.
        for bad in [json!({ "title": "   " }), json!({ "title": "a\nb" }), json!({ "title": "я".repeat(121) })] {
            let r = exec("rename_chat", bad, &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }

        // Unknown session id maps to not_found; no session in ctx and no
        // sessionId is an argument error.
        let r = exec("rename_chat", json!({ "sessionId": 999, "title": "x" }), &ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = exec("rename_chat", json!({ "title": "x" }), &ToolCtx::default());
        assert_eq!(r.error.unwrap().code, "invalid_args");

        let titles: Vec<Option<String>> = with_conn(&ctx, |conn| {
            let list = crate::harness::list_sessions(conn)?;
            Ok::<Vec<Option<String>>, AppError>(list.into_iter().map(|s| s.title).collect())
        })
        .unwrap();
        assert!(titles.contains(&Some("Переименована".to_string())));
        assert!(titles.contains(&Some("Текущая".to_string())));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn new_and_delete_chat_tools_round_trip_with_events() {
        let env = setup("ht-chat-mgmt");

        // new_chat without a title: the default «Новый чат» is stored, the id
        // and title come back, and the session really exists with its chat row.
        let r = exec("new_chat", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let created = r.data.unwrap();
        let new_id = created["sessionId"].as_i64().unwrap();
        assert_eq!(created["title"], "Новый чат");
        assert!(with_conn(&env.ctx, |conn| crate::harness::session_chat_id(conn, new_id)).is_ok());

        // Explicit title is trimmed and stored; empty/blank falls back to the default.
        let r = exec("new_chat", json!({ "title": "  Витрина  " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let titled_id = r.data.unwrap()["sessionId"].as_i64().unwrap();
        let titled = with_conn(&env.ctx, crate::harness::list_sessions)
            .unwrap()
            .into_iter()
            .find(|s| s.id == titled_id)
            .unwrap();
        assert_eq!(titled.title.as_deref(), Some("Витрина"));
        let r = exec("new_chat", json!({ "title": "   " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let blank_out = r.data.unwrap();
        assert_eq!(blank_out["title"], "Новый чат");
        let blank_id = blank_out["sessionId"].as_i64().unwrap();

        // Multiline / overlong titles are rejected before touching DB.
        for bad in [json!({ "title": "a\nb" }), json!({ "title": "я".repeat(121) })] {
            let r = exec("new_chat", bad, &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }

        // delete_chat with an explicit id: deleted:true, current:false.
        let r = exec("delete_chat", json!({ "sessionId": new_id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let out = r.data.unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["current"], false);
        assert!(matches!(
            with_conn(&env.ctx, |conn| crate::harness::session_chat_id(conn, new_id)),
            Err(AppError::Invalid(_))
        ));

        // Deleting the current session (ToolCtx session_id, no sessionId arg)
        // is allowed and reports current:true.
        let ctx = ToolCtx {
            session_id: Some(titled_id),
            ..env.ctx.clone()
        };
        let r = exec("delete_chat", json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let out = r.data.unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["current"], true);

        // Unknown id maps to not_found; non-positive ids and the missing
        // default outside a session run are argument errors.
        let r = exec("delete_chat", json!({ "sessionId": 999 }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        for bad in [json!({ "sessionId": 0 }), json!({ "sessionId": -3 }), json!({ "sessionId": "x" })] {
            let r = exec("delete_chat", bad, &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }
        let r = exec("delete_chat", json!({}), &ToolCtx::default());
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // Every successful create/delete notified the UI; failures did not:
        // 3 creates + 3 deletes succeeded.
        let r = exec("delete_chat", json!({ "sessionId": blank_id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let events = env.events.lock().unwrap().clone();
        let notified = events
            .iter()
            .filter(|(name, _)| name == "sessions-changed")
            .count();
        assert_eq!(notified, 6, "3 creates + 3 deletes must notify: {events:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
