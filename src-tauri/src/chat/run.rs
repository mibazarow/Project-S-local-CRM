// Run orchestration for both agents (SPEC 3.3, 4.6, 6): start_project_run
// persists the user turn, then spawn_run puts the agent loop on a background
// thread with its own DB connection (WAL-safe, same pattern as index/search),
// streams "agent-event" / "agent-confirmation" events to the UI, journals
// every tool call and mirrors each transcript message into the messages table
// through the loop's on_message hook. CRUD, history and message indexing stay
// in the parent module; this submodule owns only the run path.
//
// Submodules: `start` — startup, prompt build, MCP bridging and the RunBridge
// shape; `bridge` — the shared run thread, confirmation bridge, event routing
// and journaling; `persist` — the final-answer append. The items the parent
// re-exports (start_project_run, register_mcp_tools, RunBridge, spawn_run and
// the test-only persist_final_answer) are surfaced here so call sites keep the
// `chat::…` paths.
mod bridge;
mod persist;
mod start;
#[cfg(test)]
mod test_support;

pub use start::start_project_run;
pub(crate) use start::{register_mcp_tools, RunBridge};
pub(crate) use bridge::spawn_run;
// Only the harness/notes test call sites use this directly; production run
// paths reach it inside spawn_run's thread.
#[cfg(test)]
pub(crate) use persist::persist_final_answer;
