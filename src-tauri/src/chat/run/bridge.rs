// The shared run thread for both agents (SPEC 3.3, 4.6, 6): spawn_run puts the
// agent loop on a background thread with its own DB connection (WAL-safe, same
// pattern as index/search), streams "agent-event" / "agent-confirmation" events
// to the UI, journals every tool call and mirrors each transcript message into
// the messages table through the loop's on_message hook. Startup/prompt build
// lives in `start`, the final-answer append in `persist`; this file owns the
// run thread, the confirmation bridge and the event routing.
use crate::agent::r#loop as agent_loop;
use crate::chat::append_chat_message;
use crate::chat::context;
use crate::confirm::{ConfirmDecision, ConfirmRequest, Confirmer};
use crate::config::{self, AppConfig, ConfigKind, LlmConfig};
use crate::db;
use crate::error::{AppError, Result};
use crate::llm::{self, ChatMessage, ChatParams};
use crate::memory;
use crate::paths;
use crate::scheduler::{self, Notifier};
use crate::state::{AgentRegistry, AppState, RunKey};
use crate::sync_util::lock_recover;
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

use super::persist::persist_final_answer;
use super::start::RunBridge;

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
    // 6.4). The window is the active model's own value when configured, with
    // the global `contextWindow` as the legacy fallback.
    let context_window = llm_cfg.effective_context_window(&params.model);
    let ctx = {
        let guard = lock_recover(&thread_state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        context::build_context(
            conn,
            chat_id,
            &system_prompt,
            &volatile_block,
            context_window,
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
                "projectId": run.project_id(),
                "sessionId": run.session_id(),
                "notes": run.is_notes().then_some(true),
                "upToPosition": up_to_position,
                "summary": ctx.new_summary.clone().unwrap_or_default(),
            }),
        );
    }
    if let Some(message) = ctx.compaction_error.clone() {
        let _ = app.emit(
            "agent-compaction-failed",
            json!({
                "chatId": chat_id,
                "projectId": run.project_id(),
                "sessionId": run.session_id(),
                "notes": run.is_notes().then_some(true),
                "message": message,
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

// Whether the repeat detector may see this run: only successfully completed
// runs count, and scheduled runs never do (a schedule is already a reusable
// workflow, so it must not bump the counter or propose a skill).
fn repeat_detector_applies(kind: &str, finish_reason: &str) -> bool {
    kind != "schedule" && matches!(finish_reason, "finish" | "stop")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{catalog_with, finish_call, test_config, tools_outcome, ScriptedBackend};
    use super::*;
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::chat::test_support::call;
    use crate::onboarding::temp_test_dir;
    use std::fs;
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
}
