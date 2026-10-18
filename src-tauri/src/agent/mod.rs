// Agent loop core (SPEC 6.4): tool catalog, tool trait + result envelope,
// finish_task, two-level permissions, lazy tool discovery, loop driver.
pub mod browser_tool;
pub mod catalog;
pub mod clipboard_tools;
pub mod doc_tools;
pub mod edit_tools;
pub mod finish;
pub mod git_tools;
pub mod harness_tools;
pub mod hints;
pub mod image_tools;
#[cfg(test)]
pub mod invariants;
pub mod notes_tools;
pub mod ocr_tools;
pub mod pdf_tools;
pub mod process_tools;
pub mod project_tools;
pub mod prompts;
pub mod result;
pub mod screenshot_tool;
pub mod send_file;
pub mod ssh_tools;
pub mod task_glue;
pub mod tool;
pub mod undo_tools;

#[path = "loop.rs"]
pub mod r#loop;
pub mod net_tools;
