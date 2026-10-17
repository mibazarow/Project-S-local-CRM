// Chat persistence (SPEC 3.3, 4.6) plus the project-agent run bridge:
// send_project_message persists the user turn, spawns the agent loop on a
// background thread with its own DB connection (WAL-safe, same pattern as
// index/search), streams "agent-event" / "agent-confirmation" events to the
// UI, and mirrors every transcript message into the messages table through
// the loop's on_message hook — the DB copy can never diverge from what the
// model saw, and the transcript stays append-only.
pub mod context;

use crate::agent::catalog::Catalog;
use crate::agent::finish::FinishTaskTool;
use crate::agent::r#loop as agent_loop;
use crate::agent::permissions::{ConfirmDecision, ConfirmRequest, Confirmer};
use crate::agent::project_tools::project_catalog;
use crate::agent::prompts::project_system_prompt;
use crate::agent::tool::Tool;
use crate::attachments::{self, AttachmentRef};
use crate::config::{self, AppConfig, ConfigKind, LlmConfig};
use crate::db;
use crate::error::{AppError, Result};
use crate::llm::{self, ChatMessage, ChatParams, ContentPart, MessageContent, Role, ToolCall};
use crate::memory;
use crate::paths;
use crate::projects;
use crate::state::{AgentRegistry, AppState, RunKey};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

// A project-agent turn may chain several tool rounds; the cap is a safety
// net, the real end is finish_task (SPEC 6.4).
pub const MAX_STEPS: usize = 24;
// A confirmation left unanswered (user away) denies rather than hangs.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: i64,
    pub chat_id: i64,
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Vec<Value>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub created_at: String,
    pub position: i64,
    // Saved attachment copies linked to this message (SPEC 6.1); non-empty
    // only on user turns. The raw content stays the plain user text: the UI
    // renders the attachments as chips/thumbnails next to it.
    pub attachments: Vec<AttachmentRef>,
}

// Payload for the "agent-event" stream: one AgentEvent wrapped with its chat
// id plus the run routing key — projectId for project chats, sessionId for
// harness sessions (SPEC 6: one loop, one event stream, isolated contexts).
// Exactly one of the two ids is set; the other key is omitted from the JSON.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentEventPayload {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<i64>,
    // Set only for notes-chat runs: { chatId, notes: true, event }; the
    // project/harness payloads keep their exact previous shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<bool>,
    event: agent_loop::AgentEvent,
}

// Payload for the "agent-confirmation" UI event (SPEC 6: one confirmation
// queue for both agents), routed by the same run key as the event stream.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequestPayload {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i64>,
    // Routed to the notes chat UI (see AgentEventPayload.notes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<bool>,
    pub tool: String,
    pub args: Value,
    pub danger_reason: Option<String>,
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_from_str(s: &str) -> Result<Role> {
    match s {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(AppError::Db(format!("unknown message role: {other}"))),
    }
}

// Tool calls are stored as a JSON array of {id, name, arguments}; arguments
// stay the raw model string so the wire shape reconstructs exactly.
fn tool_calls_to_json(calls: &[ToolCall]) -> Result<Option<String>> {
    if calls.is_empty() {
        return Ok(None);
    }
    let arr: Vec<Value> = calls
        .iter()
        .map(|tc| json!({ "id": tc.id, "name": tc.name, "arguments": tc.raw_arguments }))
        .collect();
    Ok(Some(serde_json::to_string(&arr)?))
}

fn tool_calls_from_json(text: &str) -> Result<Vec<ToolCall>> {
    let arr: Vec<Value> = serde_json::from_str(text)?;
    Ok(arr
        .iter()
        .map(|v| {
            ToolCall::new(
                v["id"].as_str().unwrap_or_default().to_string(),
                v["name"].as_str().unwrap_or_default().to_string(),
                v["arguments"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

fn parse_tool_calls_value(text: Option<String>) -> Vec<Value> {
    match text {
        Some(t) => serde_json::from_str(&t).unwrap_or_default(),
        None => Vec::new(),
    }
}

fn project_chat_id(conn: &Connection, project_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM chats WHERE project_id = ?1 AND kind = 'project'",
            params![project_id],
            |r| r.get(0),
        )
        .optional()?)
}

// Get-or-create: the partial unique index guarantees at most one row, so a
// second call returns the existing chat (SPEC 4.6 "один непрерывный чат").
pub fn ensure_project_chat(conn: &Connection, project_id: i64) -> Result<i64> {
    if let Some(id) = project_chat_id(conn, project_id)? {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO chats (project_id, kind, created_at, updated_at)
         VALUES (?1, 'project', datetime('now','localtime'), datetime('now','localtime'))",
        params![project_id],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn append_message(
    conn: &Connection,
    chat_id: i64,
    role: Role,
    content: Option<&str>,
    tool_calls: &[ToolCall],
    tool_call_id: Option<&str>,
    name: Option<&str>,
) -> Result<MessageDto> {
    let tool_calls_json = tool_calls_to_json(tool_calls)?;
    let position: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM messages WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO messages (chat_id, role, content, tool_calls, tool_call_id, name, created_at, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now','localtime'), ?7)",
        params![chat_id, role_str(role), content, tool_calls_json, tool_call_id, name, position],
    )?;
    conn.execute(
        "UPDATE chats SET updated_at = datetime('now','localtime') WHERE id = ?1",
        params![chat_id],
    )?;
    message_dto(conn, conn.last_insert_rowid())
}

fn message_dto(conn: &Connection, id: i64) -> Result<MessageDto> {
    let mut dto = conn
        .query_row(
            "SELECT chat_id, role, content, tool_calls, tool_call_id, name, created_at, position
             FROM messages WHERE id = ?1",
            params![id],
            |r| {
                Ok(MessageDto {
                    id,
                    chat_id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    tool_calls: parse_tool_calls_value(r.get(3)?),
                    tool_call_id: r.get(4)?,
                    name: r.get(5)?,
                    created_at: r.get(6)?,
                    position: r.get(7)?,
                    attachments: Vec::new(),
                })
            },
        )
        .optional()?
        .ok_or_else(|| AppError::Db(format!("message {id} not found")))?;
    dto.attachments = attachments::load_message_attachments(conn, id)?;
    Ok(dto)
}

// Rebuilds the model-facing transcript (system prompt excluded: the loop
// regenerates it per run). User turns that carry attachments are expanded
// through attachments::build_user_turn: documents contribute their extracted
// text, and images become image_url content parts when `vision_supported`
// (otherwise the turn carries an honest note and the image is not sent). The
// stored rows keep the plain user text; only this model-facing view is expanded.
pub fn load_history(
    conn: &Connection,
    chat_id: i64,
    vision_supported: bool,
) -> Result<Vec<ChatMessage>> {
    let base = chat_project_root(conn, chat_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, role, content, tool_calls, tool_call_id, name
         FROM messages WHERE chat_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![chat_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, role, content, tool_calls, tool_call_id, name) = row?;
        let content = if role == "user" {
            expand_user_turn(conn, id, content, base.as_deref(), vision_supported)?
        } else {
            content.map(MessageContent::Text)
        };
        out.push(ChatMessage {
            role: role_from_str(&role)?,
            content,
            tool_calls: match tool_calls {
                Some(t) => tool_calls_from_json(&t)?,
                None => Vec::new(),
            },
            tool_call_id,
            name,
        });
    }
    Ok(out)
}

// Canonical project root for the chat's project; None for harness chats,
// whose attachment savedPaths are absolute anyway.
fn chat_project_root(conn: &Connection, chat_id: i64) -> Result<Option<PathBuf>> {
    Ok(conn
        .query_row(
            "SELECT p.path FROM chats c JOIN projects p ON p.id = c.project_id WHERE c.id = ?1",
            params![chat_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

// Rebuilds a user turn with its attachment blocks; messages without stored
// attachments keep their content untouched. When vision is supported and an
// image is present, the turn becomes a multimodal parts array (text part plus
// one image_url part per image); otherwise it stays a plain string with the
// document text injected and an honest note for any image.
fn expand_user_turn(
    conn: &Connection,
    message_id: i64,
    content: Option<String>,
    base: Option<&Path>,
    vision_supported: bool,
) -> Result<Option<MessageContent>> {
    let refs = attachments::load_message_attachments(conn, message_id)?;
    if refs.is_empty() {
        return Ok(content.map(MessageContent::Text));
    }
    let dtos: Vec<attachments::AttachmentDto> = refs.iter().map(attachments::ref_to_dto).collect();
    let text = content.unwrap_or_default();
    let turn = attachments::build_user_content(
        &text,
        &dtos,
        base.unwrap_or(Path::new("")),
        vision_supported,
    );
    if turn.images.is_empty() {
        return Ok(Some(MessageContent::Text(turn.text)));
    }
    let mut parts = Vec::with_capacity(turn.images.len() + 1);
    if !turn.text.is_empty() {
        parts.push(ContentPart::text(turn.text));
    }
    for url in turn.images {
        parts.push(ContentPart::image_url(url));
    }
    Ok(Some(MessageContent::Parts(parts)))
}

// UI-facing history for a chat by its id (shared by project chats and harness
// sessions).
pub fn history_by_chat(conn: &Connection, chat_id: i64) -> Result<Vec<MessageDto>> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE chat_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map(params![chat_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ids.iter().map(|id| message_dto(conn, *id)).collect()
}

// UI-facing history for the project's chat (empty when no chat exists yet).
pub fn chat_history(conn: &Connection, project_id: i64) -> Result<Vec<MessageDto>> {
    let Some(chat_id) = project_chat_id(conn, project_id)? else {
        return Ok(Vec::new());
    };
    history_by_chat(conn, chat_id)
}

// Confirmer that blocks the agent thread on a channel until the UI resolves
// the request via resolve_confirmation; a timeout denies. The `request`
// callback publishes the payload (in production: app.emit). Shared by both
// agents: the run key routes the request to the owning project or session.
pub struct ChannelConfirmer {
    pub run: RunKey,
    pub registry: Arc<AgentRegistry>,
    pub timeout: Duration,
    pub request: Box<dyn Fn(ConfirmationRequestPayload) + Send + Sync>,
}

impl Confirmer for ChannelConfirmer {
    fn confirm(&self, req: &ConfirmRequest) -> ConfirmDecision {
        if self.registry.is_allowed(&req.tool) {
            return ConfirmDecision::AllowAlways;
        }
        let (id, rx) = self.registry.register(&req.tool);
        (self.request)(ConfirmationRequestPayload {
            id,
            project_id: self.run.project_id(),
            session_id: self.run.session_id(),
            notes: self.run.is_notes().then_some(true),
            tool: req.tool.clone(),
            args: req.args.clone(),
            danger_reason: req.danger_reason.clone(),
        });
        let decision = rx.recv_timeout(self.timeout).unwrap_or(ConfirmDecision::Deny);
        self.registry.forget(id);
        decision
    }
}

// Persists the user turn (plus links to already-saved attachment copies,
// SPEC 6.1), then runs the agent loop on a background thread. Returns the
// project chat id; progress arrives as "agent-event" and
// "agent-confirmation" events.
pub fn start_project_run(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<AgentRegistry>,
    project_id: i64,
    text: &str,
    attachments_in: &[AttachmentRef],
) -> Result<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("message text is empty".into()));
    }
    let wd = state.working_dir()?;
    let (chat_id, project) = {
        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let project = projects::get_project(conn, project_id)?;
        let chat_id = ensure_project_chat(conn, project_id)?;
        // The user turn is persisted first: build_context then sees the full
        // transcript (including it) when estimating and compacting.
        let user_message = append_message(conn, chat_id, Role::User, Some(text), &[], None, None)?;
        attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
        (chat_id, project)
    };

    // The catalog is built BEFORE the prompt so the generated tool block
    // covers everything the run can see, MCP bridges included (schemas stay
    // lazy via list_available_tools/enable_tools).
    let mut catalog = project_catalog();
    catalog.register(Arc::new(FinishTaskTool), true)?;
    // MCP tools are bridged from the app-level registry: the run's catalog
    // borrows the live server clients (SPEC 6.2).
    register_mcp_tools(&mut catalog, state.mcp.lock().unwrap().tools());
    let skills = crate::skills::short_list(&wd);
    let system_prompt = project_system_prompt(&project, &skills, &catalog.summary_prompt_block());

    spawn_run(
        app,
        &wd,
        registry,
        RunBridge {
            run: RunKey::Project(project_id),
            kind: "project",
            chat_id,
            system_prompt,
            user_query: text.to_string(),
            catalog,
            notes_root: None,
        },
    );
    Ok(chat_id)
}

// MCP/built-in name collisions stay visible as hidden, not UNKNOWN
// (SPEC 6.4 "collisions are dropped noticeably").
pub(crate) fn register_mcp_tools(catalog: &mut Catalog, tools: Vec<Arc<dyn Tool>>) {
    for tool in tools {
        let name = tool.name().to_string();
        if catalog.register(tool, true).is_err() {
            catalog.hide(&name, "name collision with the built-in catalog");
        }
    }
}

// The pieces that differ between the two agents. Everything else — the run
// thread's own DB connection, config read, memory injection, context build,
// confirmation bridge, cancel wiring, event routing and transcript
// persistence — is shared by run_agent_bridge, so project chats and harness
// sessions run through one loop.
pub(crate) struct RunBridge {
    pub run: RunKey,
    // Journal run kind (SPEC 3.3): "project" | "harness" | "schedule".
    pub kind: &'static str,
    pub chat_id: i64,
    pub system_prompt: String,
    // The fresh user turn: drives which memory records are relevant for this
    // run's "[Memory]" block (SPEC 6.3).
    pub user_query: String,
    pub catalog: Catalog,
    // Canonical Notes/ vault root for notes-agent runs (None otherwise):
    // handed to the notes tools through ToolCtx.notes_root.
    pub notes_root: Option<PathBuf>,
}

// Spawns the shared agent run on a background thread for either agent.
pub(crate) fn spawn_run(
    app: &AppHandle,
    wd: &Path,
    registry: &Arc<AgentRegistry>,
    bridge: RunBridge,
) {
    let app = app.clone();
    let registry = Arc::clone(registry);
    let wd = wd.to_path_buf();
    std::thread::spawn(move || run_agent_bridge(&app, &registry, &wd, bridge));
}

// Runs the agent loop for a project chat or a harness session, emitting the
// terminal error event (routed by run key) when the run itself fails.
fn run_agent_bridge(
    app: &AppHandle,
    registry: &Arc<AgentRegistry>,
    wd: &Path,
    bridge: RunBridge,
) {
    let (run, chat_id) = (bridge.run, bridge.chat_id);
    if let Err(e) = run_agent_inner(app, registry, wd, bridge) {
        let _ = app.emit(
            "agent-event",
            AgentEventPayload {
                chat_id,
                project_id: run.project_id(),
                session_id: run.session_id(),
                notes: run.is_notes().then_some(true),
                event: agent_loop::AgentEvent::Error {
                    code: "run".into(),
                    message: e.to_string(),
                },
            },
        );
    }
}

fn run_agent_inner(
    app: &AppHandle,
    registry: &Arc<AgentRegistry>,
    wd: &Path,
    bridge: RunBridge,
) -> Result<()> {
    let RunBridge {
        run,
        kind,
        chat_id,
        mut system_prompt,
        user_query,
        mut catalog,
        notes_root,
    } = bridge;
    // The run thread owns its DB connection: a long agent turn must not
    // hold the UI connection's mutex; WAL + busy_timeout keeps both safe.
    let thread_state = Arc::new(AppState::new());
    *thread_state.working_dir.lock().unwrap() = Some(wd.to_path_buf());
    *thread_state.db.lock().unwrap() = Some(db::open(&paths::db_path(wd))?);
    // Tools that notify the UI (rename_chat, todowrite) emit through this handle.
    *thread_state.emitter.lock().unwrap() = Some(Arc::new(app.clone()));

    let llm_cfg: LlmConfig = serde_json::from_value(config::read_config(wd, ConfigKind::Llm)?)?;
    let params = ChatParams::from(&llm_cfg);
    // Manual `vision` override wins over the cached probe result; unknown
    // means images are not sent and the turn carries an honest note.
    let vision_supported = llm_cfg.vision_enabled();
    let client = llm::client_from_config(wd)?;
    let app_cfg: AppConfig =
        serde_json::from_value(config::read_config(wd, ConfigKind::App)?).unwrap_or_default();

    // Global memory injection (SPEC 6.3): the fixed identity lines + the
    // always-injected gender line + records relevant to the fresh user turn,
    // appended to the system prompt of every run (project chats and harness
    // sessions alike).
    {
        let guard = thread_state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let block = memory::context_block(conn, run.project_id(), &user_query)?;
        system_prompt.push_str(&format!("\n\n{block}"));
    }

    // Build the model-facing transcript with checkpoint compaction (SPEC 4.6,
    // 6.4); on summarization failure build_context falls back to full history.
    let ctx = {
        let guard = thread_state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        context::build_context(
            conn,
            chat_id,
            &system_prompt,
            llm_cfg.context_window,
            app_cfg.compaction_threshold_percent,
            &client,
            &params,
            vision_supported,
        )?
    };
    if let Some(up_to_position) = ctx.new_checkpoint_up_to {
        let _ = app.emit(
            "agent-context-compacted",
            json!({ "chatId": chat_id, "upToPosition": up_to_position }),
        );
    }

    let mut agent_state = agent_loop::AgentState {
        backend: &client,
        catalog: &mut catalog,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    registry.set_cancel(run, &cancel);

    let confirmer_app = app.clone();
    let confirmer = ChannelConfirmer {
        run,
        registry: Arc::clone(registry),
        timeout: CONFIRM_TIMEOUT,
        request: Box::new(move |payload| {
            let _ = confirmer_app.emit("agent-confirmation", payload);
        }),
    };
    let config = agent_loop::AgentRunConfig {
        messages: ctx.messages,
        max_steps: MAX_STEPS,
        limits: agent_loop::RunLimits::default(),
        // Full autopilot (SPEC 5.2) short-circuits the confirmation queue.
        allow_all: app_cfg.autopilot,
        project_id: run.project_id(),
        session_id: run.session_id(),
        params,
        cancel: Some(Arc::clone(&cancel)),
        // Tools reach the DB and the working dir through the run's own
        // thread state (fs/task/memory tools borrow this connection).
        state: Some(Arc::clone(&thread_state)),
        working_dir: Some(wd.to_path_buf()),
        notes_root,
    };

    let emit_app = app.clone();
    // Run/tool journal (SPEC 3.3, 6 "все вызовы пишутся в журнал запусков"):
    // every tool call of every run kind is appended to Settings/logs as a
    // masked JSONL line through the app-level journal handle.
    let journal = app.state::<AppState>().journal.lock().unwrap().clone();
    let run_info = crate::journal::RunInfo {
        kind,
        chat_id,
        project_id: run.project_id(),
        session_id: run.session_id(),
    };
    let mut tool_seq: usize = 0;
    let mut tool_started: Option<(std::time::Instant, Value)> = None;
    let mut emit = move |event: agent_loop::AgentEvent| {
        if let Some(journal) = &journal {
            match &event {
                agent_loop::AgentEvent::ToolCallStarted { args, .. } => {
                    tool_started = Some((std::time::Instant::now(), args.clone()));
                }
                agent_loop::AgentEvent::ToolCallFinished { name, ok, envelope } => {
                    tool_seq += 1;
                    let (ms, args) = match tool_started.take() {
                        Some((started_at, args)) => (started_at.elapsed().as_millis(), args),
                        None => (0, Value::Null),
                    };
                    let code = envelope
                        .get("error")
                        .and_then(|e| e.get("code"))
                        .and_then(Value::as_str);
                    let _ = journal.log_event(&run_info, tool_seq, name, *ok, code, ms, &args, envelope);
                }
                _ => {}
            }
        }
        let _ = emit_app.emit(
            "agent-event",
            AgentEventPayload {
                chat_id,
                project_id: run.project_id(),
                session_id: run.session_id(),
                notes: run.is_notes().then_some(true),
                event,
            },
        );
    };
    let persist_state = Arc::clone(&thread_state);
    let mut on_message = move |msg: &ChatMessage| {
        let guard = persist_state.db.lock().unwrap();
        if let Some(conn) = guard.as_ref() {
            let _ = append_message(
                conn,
                chat_id,
                msg.role,
                msg.text_content(),
                &msg.tool_calls,
                msg.tool_call_id.as_deref(),
                msg.name.as_deref(),
            );
        }
    };

    let result = agent_loop::run_with(&mut agent_state, &config, &confirmer, &mut emit, &mut on_message);
    registry.clear_cancel(run);
    let result = result?;
    let guard = thread_state.db.lock().unwrap();
    if let Some(conn) = guard.as_ref() {
        persist_final_answer(conn, chat_id, &result)?;
    }
    Ok(())
}

// The finish_task answer lives in the tool call arguments, not in the
// transcript, so the final assistant turn is appended after the run. A
// step_limit run already persisted its fallback text through the hook.
pub(crate) fn persist_final_answer(
    conn: &Connection,
    chat_id: i64,
    result: &agent_loop::AgentRunResult,
) -> Result<()> {
    if result.finish_reason != "step_limit" && !result.answer.trim().is_empty() {
        append_message(conn, chat_id, Role::Assistant, Some(&result.answer), &[], None, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Catalog;
    use crate::agent::r#loop::ChatBackend;
    use crate::agent::permissions::{AutoConfirmer, DenyAllConfirmer};
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::db::schema;
    use crate::llm::{ChatEvent, ChatOutcome, ChatParams, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct ScriptedBackend {
        rounds: Mutex<VecDeque<ChatOutcome>>,
        calls: AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(rounds: Vec<ChatOutcome>) -> Self {
            ScriptedBackend {
                rounds: Mutex::new(rounds.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ChatBackend for ScriptedBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .rounds
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))?;
            if let Some(content) = &outcome.content {
                on_event(ChatEvent::Delta(content.clone()));
            }
            Ok(outcome)
        }
    }

    fn text_outcome(content: &str) -> ChatOutcome {
        ChatOutcome {
            content: Some(content.into()),
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
        ChatOutcome {
            content: None,
            tool_calls: calls,
            finish_reason: Some("tool_calls".into()),
            usage: None,
        }
    }

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall::new(id.into(), name.into(), args.to_string())
    }

    fn finish_call(answer: &str) -> ToolCall {
        call("f1", "finish_task", json!({ "action": "finish", "answer": answer }))
    }

    fn setup(tag: &str) -> (std::path::PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        (wd, conn, dto.id)
    }

    fn test_config(user_message: &str) -> agent_loop::AgentRunConfig {
        agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user(user_message)],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
            },
            cancel: None,
            state: None,
            working_dir: None,
            notes_root: None,
        }
    }

    fn catalog_with(extra: Option<EchoTool>) -> Catalog {
        let mut c = Catalog::new();
        c.register(Arc::new(FinishTaskTool), true).unwrap();
        if let Some(tool) = extra {
            c.register(Arc::new(tool), true).unwrap();
        }
        c
    }

    #[test]
    fn ensure_project_chat_returns_the_same_chat_for_a_project() {
        let (wd, conn, pid) = setup("chat-one-per-project");
        let first = ensure_project_chat(&conn, pid).unwrap();
        let second = ensure_project_chat(&conn, pid).unwrap();
        assert_eq!(first, second);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE kind='project'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn history_round_trips_tool_calls_in_order() {
        let (wd, conn, pid) = setup("chat-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("hello"), &[], None, None).unwrap();
        let calls = vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        append_message(
            &conn,
            chat_id,
            Role::Tool,
            Some("{\"ok\":true}"),
            &[],
            Some("c1"),
            None,
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("done"), &[], None, None).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[1].tool_calls.len(), 1);
        assert_eq!(hist[1].tool_calls[0].id, "c1");
        assert_eq!(hist[1].tool_calls[0].name, "read_file");
        assert_eq!(hist[1].tool_calls[0].raw_arguments, "{\"rel_path\":\"a.md\"}");
        assert_eq!(hist[2].role, Role::Tool);
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].text_content(), Some("done"));

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 4);
        assert_eq!(dtos[0].role, "user");
        assert_eq!(dtos[1].tool_calls[0]["name"], "read_file");
        assert_eq!(dtos[2].tool_call_id.as_deref(), Some("c1"));
        assert!(dtos[0].position < dtos[1].position);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn send_persists_attachments_linked_to_user_message_and_history_returns_them() {
        let (wd, conn, pid) = setup("chat-attach-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("проверь счёт"), &[], None, None)
            .unwrap();
        let refs = vec![
            AttachmentRef {
                name: "счёт.docx".into(),
                saved_path: "Входящие/счёт.docx".into(),
                kind: "document".into(),
                is_image: false,
            },
            AttachmentRef {
                name: "pic.png".into(),
                saved_path: "Входящие/pic.png".into(),
                kind: "image".into(),
                is_image: true,
            },
        ];
        attachments::save_message_attachments(&conn, user.id, &refs).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("ок"), &[], None, None).unwrap();

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 2);
        assert_eq!(dtos[0].attachments.len(), 2);
        assert_eq!(dtos[0].attachments[0].name, "счёт.docx");
        assert_eq!(dtos[0].attachments[0].saved_path, "Входящие/счёт.docx");
        assert_eq!(dtos[0].attachments[0].kind, "document");
        assert!(dtos[0].attachments[1].is_image);
        assert!(dtos[1].attachments.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_expands_document_text_and_notes_images_when_vision_off() {
        let (wd, conn, pid) = setup("chat-attach-turn");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let turn = hist[0].text_content().unwrap();
        assert!(turn.starts_with("что тут?"));
        // Documents contribute their extracted text to the model turn...
        assert!(turn.contains("[Attachment: a.md]\nданные проекта"));
        // ...images are honestly noted and not sent (no base64 in the turn).
        assert!(turn.contains("[Image \"pic.png\" not sent: the model does not support vision]"));
        assert!(!turn.contains("base64"));

        // The stored row keeps the plain user text: the UI renders the
        // attachments as separate chips next to the bubble.
        let raw: String = conn
            .query_row("SELECT content FROM messages WHERE id = ?1", params![user.id], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "что тут?");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_sends_image_parts_when_vision_on() {
        let (wd, conn, pid) = setup("chat-attach-turn-vision");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, true).unwrap();
        // With vision on, the user turn is a parts array: text + image_url.
        let wire = serde_json::to_value(&hist[0]).unwrap();
        let parts = wire["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"]
            .as_str()
            .unwrap()
            .contains("[Attachment: a.md]\nданные проекта"));
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        // No honest "not sent" note when the image is actually sent.
        assert!(!serde_json::to_string(&hist[0]).unwrap().contains("not sent"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn scripted_run_persists_transcript_and_emits_events_in_order() {
        let (wd, conn, pid) = setup("chat-scripted-run");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("final answer")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let mut on_message = |msg: &ChatMessage| {
            append_message(
                &conn,
                chat_id,
                msg.role,
                msg.text_content(),
                &msg.tool_calls,
                msg.tool_call_id.as_deref(),
                msg.name.as_deref(),
            )
            .unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |e| events.push(e),
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        // Events in loop order: tool started/finished, finish_task, Done.
        let kinds: Vec<String> = events
            .iter()
            .map(|e| match e {
                agent_loop::AgentEvent::ToolCallStarted { .. } => "started".into(),
                agent_loop::AgentEvent::ToolCallFinished { .. } => "finished".into(),
                agent_loop::AgentEvent::Done { .. } => "done".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kinds, ["started", "finished", "started", "finished", "done"]);
        assert!(matches!(
            events.last(),
            Some(agent_loop::AgentEvent::Done { answer, reason })
                if answer == "final answer" && reason == "finish"
        ));

        // DB mirrors the transcript: user, asst(tool_call), tool, asst(finish), tool, asst(answer).
        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[1].tool_calls[0].name, "echo");
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].tool_calls[0].name, "finish_task");
        assert_eq!(hist[5].text_content(), Some("final answer"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn step_limit_does_not_duplicate_the_fallback_answer() {
        let (wd, conn, pid) = setup("chat-step-limit");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let backend = ScriptedBackend::new(vec![text_outcome("thought")]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.max_steps = 1;
        let mut on_message = |msg: &ChatMessage| {
            append_message(&conn, chat_id, msg.role, msg.text_content(), &msg.tool_calls, None, None)
                .unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &cfg,
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();
        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].text_content(), Some("thought"));
        fs::remove_dir_all(&wd).unwrap();
    }

    fn confirmer_with(
        registry: &Arc<AgentRegistry>,
        payloads: &Arc<Mutex<Vec<ConfirmationRequestPayload>>>,
        timeout: Duration,
    ) -> ChannelConfirmer {
        let sink = Arc::clone(payloads);
        ChannelConfirmer {
            run: RunKey::Project(1),
            registry: Arc::clone(registry),
            timeout,
            request: Box::new(move |p| sink.lock().unwrap().push(p)),
        }
    }

    fn spawn_resolver(
        registry: Arc<AgentRegistry>,
        payloads: Arc<Mutex<Vec<ConfirmationRequestPayload>>>,
        decision: ConfirmDecision,
    ) -> std::thread::JoinHandle<u64> {
        std::thread::spawn(move || {
            for _ in 0..1000 {
                if let Some(p) = payloads.lock().unwrap().first().cloned() {
                    registry.resolve(p.id, decision).unwrap();
                    return p.id;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("no confirmation request arrived");
        })
    }

    fn envelope_for(events: &[agent_loop::AgentEvent], name: &str) -> Value {
        events
            .iter()
            .find_map(|e| match e {
                agent_loop::AgentEvent::ToolCallFinished { name: n, envelope, .. } if n == name => {
                    Some(envelope.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no ToolCallFinished for {name}"))
    }

    #[test]
    fn channel_confirmer_allow_lets_the_tool_run() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(Arc::clone(&registry), Arc::clone(&payloads), ConfirmDecision::Allow);

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "ok");
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
        let p = payloads.lock().unwrap()[0].clone();
        assert_eq!(p.tool, "echo");
        assert_eq!(p.project_id, Some(1));
        assert_eq!(p.session_id, None);
        assert_eq!(p.args, json!({ "text": "hi" }));
    }

    #[test]
    fn channel_confirmer_deny_yields_denied_by_user() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(Arc::clone(&registry), Arc::clone(&payloads), ConfirmDecision::Deny);

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("recovered")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "recovered");
        assert_eq!(envelope_for(&events, "echo")["error"]["code"], "denied_by_user");
    }

    #[test]
    fn allow_always_skips_the_next_confirmation_for_that_tool() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(
            Arc::clone(&registry),
            Arc::clone(&payloads),
            ConfirmDecision::AllowAlways,
        );

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
            tools_outcome(vec![call("c2", "echo", json!({ "text": "b" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "done");
        // Only the first call produced a UI request; the second was answered
        // from the session allow-always set without emitting.
        assert_eq!(payloads.lock().unwrap().len(), 1);
        assert!(registry.is_allowed("echo"));
        let finished: Vec<&Value> = events
            .iter()
            .filter_map(|e| match e {
                agent_loop::AgentEvent::ToolCallFinished { name, envelope, .. } if name == "echo" => {
                    Some(envelope)
                }
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 2);
        assert!(finished.iter().all(|e| e["ok"] == json!(true)));
    }

    #[test]
    fn confirmation_timeout_denies() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        // Nobody resolves: the receiver times out and the decision is Deny.
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_millis(50));
        let req = ConfirmRequest {
            tool: "echo".into(),
            args: json!({ "text": "x" }),
            project_id: Some(1),
            danger_reason: None,
        };
        assert_eq!(confirmer.confirm(&req), ConfirmDecision::Deny);
        assert_eq!(payloads.lock().unwrap().len(), 1);
        // The pending entry was dropped after the timeout.
        assert_eq!(registry.pending_count(), 0);
    }

    #[test]
    fn autopilot_bypasses_the_confirmation_bridge() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.allow_all = true;
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &cfg,
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.answer, "done");
        assert!(payloads.lock().unwrap().is_empty());
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
    }

    #[test]
    fn resolve_confirmation_rejects_unknown_ids() {
        let registry = AgentRegistry::new();
        assert!(registry.resolve(999, ConfirmDecision::Allow).is_err());
    }

    #[test]
    fn event_payload_routes_by_run_key() {
        let ev = agent_loop::AgentEvent::Done {
            answer: "x".into(),
            reason: "finish".into(),
        };
        let project = serde_json::to_value(AgentEventPayload {
            chat_id: 7,
            project_id: RunKey::Project(3).project_id(),
            session_id: RunKey::Project(3).session_id(),
            notes: RunKey::Project(3).is_notes().then_some(true),
            event: ev.clone(),
        })
        .unwrap();
        assert_eq!(project["chatId"], 7);
        assert_eq!(project["projectId"], 3);
        assert!(project.get("sessionId").is_none(), "project run leaked sessionId");
        assert!(project.get("notes").is_none(), "project run leaked notes flag");

        let session = serde_json::to_value(AgentEventPayload {
            chat_id: 8,
            project_id: RunKey::Session(4).project_id(),
            session_id: RunKey::Session(4).session_id(),
            notes: RunKey::Session(4).is_notes().then_some(true),
            event: ev.clone(),
        })
        .unwrap();
        assert_eq!(session["sessionId"], 4);
        assert!(session.get("projectId").is_none(), "session run leaked projectId");
        assert!(session.get("notes").is_none(), "session run leaked notes flag");

        let notes = serde_json::to_value(AgentEventPayload {
            chat_id: 9,
            project_id: RunKey::Notes.project_id(),
            session_id: RunKey::Notes.session_id(),
            notes: RunKey::Notes.is_notes().then_some(true),
            event: ev,
        })
        .unwrap();
        assert_eq!(notes["chatId"], 9);
        assert_eq!(notes["notes"], true);
        assert!(notes.get("projectId").is_none(), "notes run leaked projectId");
        assert!(notes.get("sessionId").is_none(), "notes run leaked sessionId");
    }

    #[test]
    fn cancel_flag_stops_a_run_and_keeps_completed_steps_persisted() {
        let (wd, conn, pid) = setup("chat-cancel");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let backend = ScriptedBackend::new(vec![tools_outcome(vec![finish_call("never")])]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.cancel = Some(Arc::clone(&cancel));
        let result = agent_loop::run_with(&mut state, &cfg, &AutoConfirmer::allowing(true), |_| {}, |_| {})
            .unwrap();
        assert_eq!(result.finish_reason, "cancelled");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        persist_final_answer(&conn, chat_id, &result).unwrap();
        assert!(load_history(&conn, chat_id, false).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
}
