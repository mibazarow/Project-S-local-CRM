// Agent loop core (SPEC 6.4): tool catalog, tool trait + result envelope,
// finish_task, two-level permissions, lazy tool discovery, loop driver.
// Consumed by the chat UI slice (M3c), not wired into commands yet.
#![allow(dead_code)]

pub mod browser_tool;
pub mod catalog;
pub mod doc_tools;
pub mod edit_tools;
pub mod finish;
pub mod git_tools;
pub mod harness_tools;
pub mod hints;
pub mod permissions;
pub mod project_tools;
pub mod prompts;
pub mod result;
pub mod tool;

#[path = "loop.rs"]
pub mod r#loop;
pub mod net_tools;
