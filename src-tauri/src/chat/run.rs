// Run orchestration for both agents (SPEC 3.3, 4.6, 6): start_project_run
// persists the user turn, then spawn_run puts the agent loop on a background
// thread with its own DB connection (WAL-safe, same pattern as index/search),
// streams "agent-event" / "agent-confirmation" events to the UI, journals
// every tool call and mirrors each transcript message into the messages table
// through the loop's on_message hook. CRUD, history and message indexing stay
// in the parent module; this file owns only the run path.
use crate::agent::catalog::Catalog;
use crate::sync_util::lock_recover;
use crate::agent::r#loop as agent_loop;
use crate::confirm::{ConfirmDecision, ConfirmRequest, Confirmer};
use crate::agent::project_tools::project_catalog;
use crate::agent::prompts::project_system_prompt;
use crate::agent::tool::Tool;
use crate::attachments::{self, AttachmentRef};
use crate::config::{self, AppConfig, ConfigKind, LlmConfig};
use crate::db;
use crate::error::{AppError, Result};
use crate::llm::{self, ChatMessage, ChatParams, Role};
use crate::memory;
use crate::paths;
use crate::projects;
use crate::scheduler::{self, Notifier};
use crate::state::{AgentRegistry, AppState, RunKey};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use super::context;
use super::{append_chat_message, append_full, append_message, ensure_project_chat};

// A project-agent turn may chain several tool rounds; the cap is a safety
// net, the real end is finish_task (SPEC 6.4).
pub const MAX_STEPS: usize = 1000;
// A confirmation left unanswered (user away) denies rather than hangs.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(600);

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
    let (chat_id, project, brief, memory_records, related_line) = {
        let guard = lock_recover(&state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let project = projects::get_project(conn, project_id)?;
        let chat_id = ensure_project_chat(conn, project_id)?;
        // The user turn is persisted first: build_context then sees the full
        // transcript (including it) when estimating and compacting.
        let user_message = append_message(conn, chat_id, Role::User, Some(text), &[], None, None)?;
        attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
        // Cached project brief (brief.rs): read-only here; generation/refresh
        // happens on project open and file changes. The brief and the
        // query-relevant memory records are volatile: they ride the
        // [Context] block of the fresh user turn, never the system prompt.
        let brief = crate::brief::get_brief(conn, project_id)?;
        let memory_records = memory::relevant_records(conn, Some(project_id), text)?;
        // One short "Related" line for the entities linked to this project
        // (links slice); empty when nothing is linked, so volatile_block
        // drops the section.
        let related_line = crate::links::context_line(conn, "project", &project_id.to_string(), 8);
        (chat_id, project, brief, memory_records, related_line)
    };

    // The catalog is built BEFORE the prompt so the generated tool block
    // covers everything the run can see, MCP bridges included (schemas stay
    // lazy via list_available_tools/enable_tools).
    let mut catalog = project_catalog();
    // MCP tools are bridged from the app-level registry: the run's catalog
    // borrows the live server clients (SPEC 6.2).
    register_mcp_tools(&mut catalog, lock_recover(&state.mcp).tools());
    let skills = crate::skills::short_list(&wd);
    // Stable system prompt only: identical bytes for every turn of the
    // project, whatever the brief or the memory query says.
    let system_prompt = project_system_prompt(&project, &skills, &catalog.summary_prompt_block());
    let brief_section = match brief.as_ref() {
        Some(b) => format!("Project brief (updated {}):\n{}", b.updated_at, b.text),
        None => "Project brief:\nNo brief yet.".to_string(),
    };
    let volatile_block = context::volatile_block(&[&brief_section, &memory_records, &related_line]);

    spawn_run(
        app,
        &wd,
        registry,
        RunBridge {
            run: RunKey::Project(project_id),
            kind: "project",
            chat_id,
            user_text: text.to_string(),
            system_prompt,
            volatile_block,
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
    // The fresh user turn that started this run: the repeat detector's
    // signature input (SPEC 6.2 Q20 skill proposals).
    pub user_text: String,
    // Stable system prompt: identical bytes across the turns of a chat, so
    // the provider's prompt cache keeps its prefix (system + history).
    pub system_prompt: String,
    // The volatile [Context] block: project brief (with its updated
    // timestamp), memory records relevant to the fresh user turn, and the
    // notes vault summary. build_context prepends it to the last user
    // message of the model-facing view only — the stored row stays plain.
    pub volatile_block: String,
    pub catalog: Catalog,
    // Canonical Notes/ vault root for notes-agent runs (None otherwise):
    // handed to the notes tools through ToolCtx.notes_root.
    pub notes_root: Option<PathBuf>,
}

// Fixed per-agent sampling temperature, applied to every run regardless of
// llm.json (the Settings UI no longer exposes it): notes run a touch warmer,
// project/harness/scheduled runs stay deterministic.
pub(crate) fn temperature_for_kind(kind: &str) -> f32 {
    match kind {
        "notes" => 0.4,
        _ => 0.2,
    }
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

// app.json for the run path: a malformed file fails the run with a typed
// error (surfaced through the normal run error event) instead of silently
// falling back to defaults; a missing file still yields the shipped defaults.
fn run_app_config(wd: &Path) -> Result<AppConfig> {
    serde_json::from_value(config::read_config(wd, ConfigKind::App)?)
        .map_err(|e| AppError::InvalidConfig(format!("app.json: {e}")))
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
        user_text,
        mut system_prompt,
        volatile_block,
        mut catalog,
        notes_root,
    } = bridge;
    // Wall clock of the whole run: the repeat detector's complexity input
    // (SPEC 6.2 Q20); a run of 2+ minutes is complex whatever its tool mix.
    let started = std::time::Instant::now();
    // The run thread owns its DB connection: a long agent turn must not
    // hold the UI connection's mutex; WAL + busy_timeout keeps both safe.
    let thread_state = Arc::new(AppState::new());
    *lock_recover(&thread_state.working_dir) = Some(wd.to_path_buf());
    *lock_recover(&thread_state.db) = Some(db::open(&paths::db_path(wd))?);
    // Tools that notify the UI (rename_chat, todowrite) emit through this handle.
    *lock_recover(&thread_state.emitter) = Some(Arc::new(app.clone()));

    let llm_cfg: LlmConfig = serde_json::from_value(config::read_config(wd, ConfigKind::Llm)?)?;
    // Temperature is fixed per agent (project/harness/schedule 0.2, notes 0.4)
    // and always overrides whatever llm.json carries, so every run path —
    // including scheduled ones — sends the same value.
    let params = ChatParams {
        temperature: Some(temperature_for_kind(kind)),
        ..ChatParams::from(&llm_cfg)
    };
    // Manual `vision` override wins over the cached probe result; unknown
    // means images are not sent and the turn carries an honest note.
    let vision_supported = llm_cfg.vision_enabled();
    let client = llm::client_from_config(wd)?;
    let app_cfg = run_app_config(wd)?;

    // Global memory (SPEC 6.3): only the stable part — the fixed identity
    // lines, the always-injected gender line and the style-of-communication
    // instruction (SPEC 6.6) — is appended to the system prompt of every run
    // (project chats, notes and harness sessions alike); it is the same for any
    // query, so the cacheable prefix survives. The records relevant to the
    // fresh user turn already ride the volatile block.
    //
    // The style profile is folded from the fresh user turn first (skipped for
    // scheduled runs, whose "user" text is the agent's own schedule prompt), so
    // the instruction this run injects already reflects the current message.
    {
        let guard = lock_recover(&thread_state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        if kind != "schedule" {
            let _ = crate::style::observe_user_message(conn, &user_text);
        }
        system_prompt.push_str(&format!("\n\n{}", memory::identity_block(conn)?));
    }

    // Build the model-facing transcript with checkpoint compaction (SPEC 4.6,
    // 6.4); on summarization failure build_context falls back to full history.
    let ctx = {
        let guard = lock_recover(&thread_state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        context::build_context(
            conn,
            chat_id,
            &system_prompt,
            &volatile_block,
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
            json!({
                "chatId": chat_id,
                "upToPosition": up_to_position,
                "summary": ctx.new_summary.unwrap_or_default(),
            }),
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
        chat_id: Some(chat_id),
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
    let journal = lock_recover(&app.state::<AppState>().journal).clone();
    let run_info = crate::journal::RunInfo {
        kind,
        chat_id,
        project_id: run.project_id(),
        session_id: run.session_id(),
    };
    let mut tool_seq: usize = 0;
    let mut tool_started: Option<(std::time::Instant, Value)> = None;
    // Completed tool call names in order (finish_task excluded): the other
    // half of the repeat-detector signature (SPEC 6.2 Q20). The closure
    // captures the borrow, not the Vec, so the names are readable after the
    // run once the closure is dropped.
    let mut tool_names: Vec<String> = Vec::new();
    let collected = &mut tool_names;
    let mut emit = move |event: agent_loop::AgentEvent| {
        if let agent_loop::AgentEvent::ToolCallFinished { name, .. } = &event {
            if name != "finish_task" {
                collected.push(name.clone());
            }
        }
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
        let guard = lock_recover(&persist_state.db);
        if let Some(conn) = guard.as_ref() {
            let _ = append_chat_message(conn, chat_id, msg);
        }
    };

    let result = agent_loop::run_with(&mut agent_state, &config, &confirmer, &mut emit, &mut on_message);
    // Release the closure's borrow of tool_names before reading them below.
    drop(emit);
    registry.clear_cancel(run);
    let result = result?;
    let guard = lock_recover(&thread_state.db);
    if let Some(conn) = guard.as_ref() {
        // The chat may be gone by the end of the run: the agent deleted its
        // own session (delete_chat) or the user removed it mid-run. The
        // transcript appends above already failed FK-safe and were dropped;
        // skip the final answer the same way instead of surfacing a run
        // error for a chat nobody can open anymore.
        let chat_exists = conn
            .query_row(
                "SELECT count(*) FROM chats WHERE id = ?1",
                params![chat_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|rows| rows > 0)
            .unwrap_or(false);
        if chat_exists {
            persist_final_answer(conn, chat_id, &result)?;
            // Q.md item 7: a finished scheduled run (run_at/cron) marks its
            // schedule unread and reports the result through a macOS
            // notification; clicking it opens this session's chat. Delivery
            // failures are ignored — the unread pulse is the safety net.
            if kind == "schedule" {
                if let Some(session_id) = run.session_id() {
                    if let Ok(Some(notice)) = scheduler::mark_run_completion(
                        conn,
                        session_id,
                        &result.answer,
                        &result.finish_reason,
                        scheduler::now_utc(),
                    ) {
                        let notifier = scheduler::TauriNotifier::new(app);
                        let _ = notifier.notify_completion(
                            &notice.title,
                            &notice.body,
                            notice.schedule_id,
                            notice.session_id,
                        );
                    }
                }
            }
            // Repeat detector (SPEC 6.2 Q20): only successfully completed
            // runs count; a detector failure never fails a finished run.
            // Scheduled runs are excluded entirely — a schedule is already a
            // reusable workflow, so it must neither bump the counter nor
            // spawn a skill proposal.
            if repeat_detector_applies(kind, &result.finish_reason) {
                let summary = crate::skills::RunSummary {
                    chat_id,
                    project_id: run.project_id(),
                    session_id: run.session_id(),
                    user_text: &user_text,
                    tool_names: &tool_names,
                    duration_secs: started.elapsed().as_secs(),
                };
                if let Ok(Some(proposal)) = crate::skills::record_run(conn, wd, &summary) {
                    let _ = app.emit(
                        "agent-skill-proposal",
                        crate::skills::proposal_event_payload(
                            chat_id,
                            run.project_id(),
                            run.session_id(),
                            run.is_notes(),
                            &proposal,
                        ),
                    );
                }
            }
        }
    }
    Ok(())
}

// The finish_task answer lives in the tool call arguments, not in the
// transcript, so the final assistant turn is appended after the run. A
// step_limit run already persisted its fallback text through the hook; its
// answer is the explicit stop notice naming the limit, so it is persisted too.
pub(crate) fn persist_final_answer(
    conn: &Connection,
    chat_id: i64,
    result: &agent_loop::AgentRunResult,
) -> Result<()> {
    if !result.answer.trim().is_empty() {
        append_full(
            conn,
            chat_id,
            Role::Assistant,
            Some(&result.answer),
            &[],
            None,
            None,
            result.reasoning.as_deref(),
        )?;
    }
    Ok(())
}

// Whether the repeat detector may see this run: only successfully completed
// runs count, and scheduled runs never do (a schedule is already a reusable
// workflow, so it must not bump the counter or propose a skill).
fn repeat_detector_applies(kind: &str, finish_reason: &str) -> bool {
    kind != "schedule" && matches!(finish_reason, "finish" | "stop")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::{Catalog, Resolution};
    use crate::agent::finish::FinishTaskTool;
    use crate::agent::r#loop::ChatBackend;
    use crate::agent::result::ToolResult;
    use crate::agent::tool::{EchoTool, PermissionPolicy, ToolCtx};
    use crate::confirm::{AutoConfirmer, DenyAllConfirmer};
    use crate::chat::test_support::{call, setup};
    use crate::chat::{chat_history, load_history};
    use crate::llm::{ChatEvent, ChatOutcome, ChatParams, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn run_temperature_is_fixed_per_agent_kind() {
        assert_eq!(temperature_for_kind("project"), 0.2);
        assert_eq!(temperature_for_kind("harness"), 0.2);
        assert_eq!(temperature_for_kind("schedule"), 0.2);
        assert_eq!(temperature_for_kind("notes"), 0.4);

        // Whatever the kind, the fixed value actually reaches the request body.
        for kind in ["project", "harness", "schedule", "notes"] {
            let params = ChatParams {
                model: "m".into(),
                temperature: Some(temperature_for_kind(kind)),
                max_tokens: None,
                reasoning_effort: None,
                requires_reasoning_passback: false,
            };
            let body = crate::llm::build_body(&[ChatMessage::user("hi")], None, &params, false);
            assert_eq!(body["temperature"], json!(temperature_for_kind(kind)));
        }
    }

    #[test]
    fn run_app_config_parses_valid_app_json_exactly_as_before() {
        let dir = temp_test_dir("chat-run-app-config-valid");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        fs::write(
            config::file_path(&dir, ConfigKind::App),
            r#"{ "autopilot": true, "compactionThresholdPercent": 55, "sessionTtlDays": 5, "agentName": "Вася" }"#,
        )
        .unwrap();
        let cfg = run_app_config(&dir).unwrap();
        assert!(cfg.autopilot);
        assert_eq!(cfg.compaction_threshold_percent, 55);
        assert_eq!(cfg.session_ttl_days, 5);
        // Absent keys keep the same defaults the old silent path produced.
        assert_eq!(cfg.tmp_retention_days, 7);
        assert_eq!(cfg.log_retention_days, 14);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_app_config_fails_on_malformed_app_json_instead_of_defaulting() {
        let dir = temp_test_dir("chat-run-app-config-bad");
        fs::create_dir_all(paths::config_dir(&dir)).unwrap();
        // Valid JSON, wrong value type: previously silently replaced by defaults.
        fs::write(config::file_path(&dir, ConfigKind::App), r#"{ "autopilot": "yes" }"#).unwrap();
        let err = run_app_config(&dir).unwrap_err();
        assert!(matches!(err, AppError::InvalidConfig(_)), "{err}");
        assert!(err.to_string().starts_with("invalid_config: app.json"), "{err}");

        // Broken JSON syntax already failed the run before: behavior unchanged.
        fs::write(config::file_path(&dir, ConfigKind::App), "{ broken").unwrap();
        assert!(matches!(run_app_config(&dir), Err(AppError::Json(_))));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_app_config_missing_file_keeps_shipped_defaults() {
        let dir = temp_test_dir("chat-run-app-config-missing");
        let cfg = run_app_config(&dir).unwrap();
        assert!(!cfg.autopilot);
        assert_eq!(cfg.compaction_threshold_percent, 80);
        fs::remove_dir_all(&dir).unwrap();
    }

    // MCP/built-in name collisions must stay visible as Hidden, and the
    // non-colliding bridged tools register as core — the run path relies
    // on both (SPEC 6.4).
    struct BridgeTool;

    impl Tool for BridgeTool {
        fn name(&self) -> &str {
            "mcp_bridge"
        }
        fn description(&self) -> &str {
            "Test MCP bridge stand-in"
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        }
        fn execute(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
            Ok(ToolResult::ok(json!({})))
        }
    }

    #[test]
    fn register_mcp_tools_hides_collisions_and_registers_the_rest_as_core() {
        let mut catalog = Catalog::new();
        catalog
            .register(Arc::new(EchoTool::new(PermissionPolicy::Skip)), true)
            .unwrap();
        register_mcp_tools(
            &mut catalog,
            vec![
                Arc::new(EchoTool::new(PermissionPolicy::Skip)),
                Arc::new(BridgeTool),
            ],
        );
        // Name collision: dropped noticeably, not silently shadowed.
        assert!(matches!(catalog.resolve("echo"), Resolution::Hidden(_)));
        // The non-colliding bridge tool joins the run catalog as a core
        // tool, visible to the model right away (same as before the split).
        assert!(matches!(
            catalog.resolve("mcp_bridge"),
            Resolution::Found(_)
        ));
        let active: Vec<String> = catalog.to_specs().into_iter().map(|s| s.name).collect();
        assert!(active.contains(&"mcp_bridge".to_string()));
    }

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
            reasoning: None,
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
        }
    }

    fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
        ChatOutcome {
            content: None,
            reasoning: None,
            tool_calls: calls,
            finish_reason: Some("tool_calls".into()),
            usage: None,
        }
    }

    fn finish_call(answer: &str) -> ToolCall {
        call("f1", "finish_task", json!({ "action": "finish", "answer": answer }))
    }

    fn test_config(user_message: &str) -> agent_loop::AgentRunConfig {
        agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user(user_message)],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            chat_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                requires_reasoning_passback: false,
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
    fn scripted_run_persists_transcript_and_emits_events_in_order() {
        let (wd, conn, pid) = setup("chat-scripted-run");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let mut thinking_round = tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]);
        thinking_round.reasoning = Some("planning the echo".into());
        let backend = ScriptedBackend::new(vec![
            thinking_round,
            tools_outcome(vec![finish_call("final answer")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
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
            Some(agent_loop::AgentEvent::Done { answer, reason, .. })
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
        // Thinking traces persist with their turns: the tool-call turn keeps
        // its own trace, and the finish_task answer turn carries the trace
        // accumulated over the run, so a follow-up request can replay it.
        assert_eq!(hist[1].reasoning_content.as_deref(), Some("planning the echo"));
        assert_eq!(hist[2].reasoning_content, None);
        assert_eq!(hist[5].reasoning_content.as_deref(), Some("planning the echo"));
        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos[1].reasoning.as_deref(), Some("planning the echo"));
        assert_eq!(dtos[5].reasoning.as_deref(), Some("planning the echo"));
        fs::remove_dir_all(&wd).unwrap();
    }

    // Backend whose rounds can fail, driving the after-tool LLM-failure path.
    struct FlakyBackend {
        outcomes: Mutex<VecDeque<std::result::Result<ChatOutcome, AppError>>>,
    }

    impl ChatBackend for FlakyBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(AppError::Invalid("flaky backend exhausted".into())))
        }
    }

    #[test]
    fn llm_error_after_a_successful_tool_persists_the_warning_in_the_transcript() {
        let (wd, conn, pid) = setup("chat-llm-error-after-tool");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let backend = FlakyBackend {
            outcomes: Mutex::new(
                vec![
                    Ok(tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))])),
                    Err(AppError::Http(
                        "HTTP 400: reasoning_content must be passed back".into(),
                    )),
                ]
                .into(),
            ),
        };
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        assert_eq!(result.finish_reason, "llm_error");
        persist_final_answer(&conn, chat_id, &result).unwrap();

        // The tool result and the warning both survive: user, asst(tool_call),
        // tool, asst(warning) — the completed action is never lost.
        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        let warning = hist[3].text_content().unwrap();
        assert!(warning.contains("Действие выполнено, но ответ модели не получен"));
        assert!(warning.contains("HTTP 400"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn step_limit_persists_the_fallback_once_plus_the_stop_notice() {
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
        // The latched fallback is persisted once through the hook, and the
        // run ends with an explicit stop notice naming the limit.
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].text_content(), Some("thought"));
        assert_eq!(hist[1].text_content(), Some("Остановлен по лимиту шагов: 1"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn final_answer_is_persisted_exactly_once() {
        // The streamed deltas are display-only. When a round streams its answer
        // as content AND ends on finish_task with the same text (the normal
        // streaming shape), the content is latched as the fallback, not written
        // as its own turn, so persist_final_answer writes the answer exactly
        // once — no duplicate copy of the streamed text.
        let (wd, conn, pid) = setup("chat-final-once");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let mut streamed_finish = tools_outcome(vec![finish_call("Привет!")]);
        streamed_finish.content = Some("Привет!".into());
        let backend = ScriptedBackend::new(vec![streamed_finish]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let answer_copies = hist
            .iter()
            .filter(|m| m.role == Role::Assistant && m.text_content() == Some("Привет!"))
            .count();
        assert_eq!(answer_copies, 1, "final answer must be persisted exactly once");
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
            reasoning: None,
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
    fn repeat_detector_skips_scheduled_and_unfinished_runs() {
        // Completed interactive runs feed the detector; scheduled runs never
        // do (no counter bump, no proposal), and unfinished reasons never do.
        assert!(repeat_detector_applies("project", "finish"));
        assert!(repeat_detector_applies("harness", "stop"));
        assert!(repeat_detector_applies("notes", "finish"));
        assert!(!repeat_detector_applies("schedule", "finish"));
        assert!(!repeat_detector_applies("schedule", "stop"));
        assert!(!repeat_detector_applies("project", "step_limit"));
        assert!(!repeat_detector_applies("project", "cancelled"));
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
