// Run startup for the project agent (SPEC 3.3, 4.6, 6): start_project_run
// persists the user turn (plus links to already-saved attachment copies,
// SPEC 6.1), builds the stable system prompt and the volatile [Context] block,
// then hands the shared RunBridge to spawn_run. MCP bridging and the RunBridge
// shape live here; the run thread itself lives in `bridge`.
use crate::agent::catalog::Catalog;
use crate::agent::prompts::project_system_prompt;
use crate::agent::project_tools::project_catalog;
use crate::agent::tool::Tool;
use crate::attachments::{self, AttachmentRef};
use crate::chat::context;
use crate::chat::{append_message, ensure_project_chat};
use crate::error::{AppError, Result};
use crate::llm::Role;
use crate::memory;
use crate::projects;
use crate::state::{AgentRegistry, AppState, RunKey};
use crate::sync_util::lock_recover;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::AppHandle;

use super::bridge::spawn_run;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::agent::result::ToolResult;
    use crate::agent::tool::{EchoTool, PermissionPolicy, ToolCtx};
    use serde_json::{json, Value};

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
}
