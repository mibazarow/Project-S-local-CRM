// Chat persistence (SPEC 3.3, 4.6): CRUD, model-facing history, the message
// search mirrors (FTS + vectors) and the message-level chat UX. The items of
// the submodules are re-exported here so call sites keep the `chat::…` paths.
// The run orchestration — send_project_message persisting the user turn, the
// agent loop on a background thread with its own DB connection (WAL-safe, same
// pattern as index/search), the "agent-event" / "agent-confirmation" streams
// and the transcript mirroring through the loop's on_message hook — lives in
// the `run` submodule; its items are re-exported below too. The DB copy can
// never diverge from what the model saw, and the transcript stays append-only.
//
// Submodules: `store` — persistence CRUD and the message-level transcript
// operations (pin / truncate / branch); `history` — model-facing and
// UI-facing transcript reads; `search` — the FTS/vector mirrors and the
// in-chat LIKE search; `types` — the DTO shapes; `context` — run context
// building; `run` — the run orchestration.
pub mod context;
mod run;

mod history;
mod search;
mod store;
mod types;

pub use run::start_project_run;
pub(crate) use run::{register_mcp_tools, RunBridge, spawn_run};
// Only the harness/notes test call sites use this directly; production run
// paths reach it inside spawn_run's thread.
#[cfg(test)]
pub(crate) use run::persist_final_answer;

pub use history::{chat_history, history_by_chat, load_history};
pub use search::{index_chat_messages, search_messages};
pub(crate) use search::index_chat_messages_with;
// Only the search test call sites use this directly; the append path reaches
// it inside `search` itself.
#[cfg(test)]
pub(crate) use search::store_message_vector;
pub use store::{
    append_chat_message, append_message, branch_chat, chat_working_dir, ensure_project_chat,
    set_pinned, truncate_after,
};
// run.rs reaches the append internals through `super::…`.
pub(crate) use store::append_full;
#[cfg(test)]
pub(crate) use store::test_support;
pub use types::{ChatBranchDto, ChatSearchHitDto, MessageDto};
