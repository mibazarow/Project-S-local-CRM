// Project-agent tool set (SPEC 4.6): search/read/write inside the project
// folder, task CRUD, and datas.md maintenance, on top of the agent core.
// Every path argument is resolved strictly inside the project root by the
// fs_ops resolvers; absolute OS paths are never exposed to the model.
// (R-12a) The set is split by concern into submodules: files (search/read/
// list/write/create_folder/rename/delete), tasks (task_*), datas (datas_*) and
// support (the shared ctx/conn/emit/envelope plumbing). This facade keeps
// project_catalog and re-exports the shared helpers unchanged, so the external
// import paths (super::project_tools::…) keep working.
mod datas;
mod files;
mod support;
mod tasks;
#[cfg(test)]
mod test_harness;

pub use datas::{DatasAddTool, DatasReadTool};
pub use files::{
    CreateFolderTool, DeletePathTool, ListTreeTool, ReadFileTool, RenamePathTool, SearchTool,
    WriteFileTool,
};
pub use tasks::{TaskCreateTool, TaskDeleteTool, TaskListTool, TaskMoveTool, TaskUpdateTool};
pub(crate) use support::{
    emit_fs_changed_for_abs, emit_projects_changed, emit_tasks_changed, emit_trash_changed,
    emit_versions_changed, envelope, require_wd, state_of, with_conn,
};

use super::catalog::Catalog;
use super::tool::Tool;
use std::sync::Arc;

// All project tools are core: the project agent's whole tool set is active
// from the first round (lazy discovery stays for the harness agent, 5.2).
pub fn project_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(SearchTool),
        Arc::new(ReadFileTool),
        Arc::new(ListTreeTool),
        Arc::new(WriteFileTool),
        // Precise edits: exact string replacement and unified-diff patches
        // via git apply; see edit_tools.rs for the safety rules.
        Arc::new(crate::agent::edit_tools::EditFileTool::project()),
        Arc::new(crate::agent::edit_tools::ApplyPatchTool),
        // Native document generation (DOCX/PDF/MD/TXT/HTML + XLSX/CSV) with
        // pure Rust writers; see doc_tools.rs for the safety rules.
        Arc::new(crate::agent::doc_tools::CreateDocumentTool::project()),
        Arc::new(crate::agent::doc_tools::CreateSpreadsheetTool::project()),
        // OCR of images and PDFs via the macOS Vision framework through an
        // embedded Swift helper; see ocr_tools.rs for the safety rules.
        Arc::new(crate::agent::ocr_tools::OcrTool),
        // PDF merge/split/fill-form via Apple PDFKit through an embedded Swift
        // helper; see pdf_tools.rs for the safety rules.
        Arc::new(crate::agent::pdf_tools::PdfMergeTool),
        Arc::new(crate::agent::pdf_tools::PdfSplitTool),
        Arc::new(crate::agent::pdf_tools::PdfFillFormTool),
        // Image resize/crop/rotate/convert via the macOS sips CLI; in-place
        // calls confirm, new files inside the project skip; see image_tools.rs.
        Arc::new(crate::agent::image_tools::ImageTransformTool),
        Arc::new(CreateFolderTool),
        Arc::new(RenamePathTool),
        Arc::new(DeletePathTool),
        Arc::new(TaskListTool),
        Arc::new(TaskCreateTool),
        Arc::new(TaskUpdateTool),
        Arc::new(TaskMoveTool),
        Arc::new(TaskDeleteTool),
        Arc::new(DatasAddTool),
        Arc::new(DatasReadTool),
        // Git tools via the system git CLI (read-only core set + confirmed
        // mutating set); see git_tools.rs for the safety rules.
        Arc::new(crate::agent::git_tools::GitStatusTool),
        Arc::new(crate::agent::git_tools::GitDiffTool),
        Arc::new(crate::agent::git_tools::GitLogTool),
        Arc::new(crate::agent::git_tools::GitBranchTool),
        Arc::new(crate::agent::git_tools::GitCommitTool),
        Arc::new(crate::agent::git_tools::GitPullTool),
        Arc::new(crate::agent::git_tools::GitPushTool),
        // SSH tools via the system ssh/scp/ssh-keygen CLI: keys, ssh-agent
        // and askpass passwords, profiles from ssh.json and the project's
        // datas.md; every mutating call is confirmed; see ssh_tools.rs.
        Arc::new(crate::agent::ssh_tools::SshListProfilesTool),
        Arc::new(crate::agent::ssh_tools::SshExecTool),
        Arc::new(crate::agent::ssh_tools::SshCopyTool::upload()),
        Arc::new(crate::agent::ssh_tools::SshCopyTool::download()),
        Arc::new(crate::agent::ssh_tools::SshGenerateKeyTool),
        Arc::new(crate::agent::ssh_tools::SshInstallKeyTool),
        Arc::new(crate::agent::ssh_tools::SshSaveProfileToDatasTool),
        // macOS clipboard read/write via the system pbcopy/pbpaste; every
        // call hits the general pasteboard; see clipboard_tools.rs.
        Arc::new(crate::agent::clipboard_tools::ClipboardGetTool::default()),
        Arc::new(crate::agent::clipboard_tools::ClipboardSetTool::default()),
        Arc::new(crate::agent::net_tools::WebfetchTool::new()),
        Arc::new(crate::agent::net_tools::WebsearchTool::new()),
        Arc::new(crate::agent::net_tools::HttpRequestTool),
        Arc::new(crate::skills::SkillTool),
        Arc::new(crate::skills::CreateSkillTool),
        Arc::new(crate::skills::SkillExecTool),
        // Global memory is shared with the harness agent (SPEC 6.3).
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        Arc::new(crate::memory::ForgetTool),
        // Per-conversation plan panel (SPEC: todowrite/todoread).
        Arc::new(crate::todos::TodoWriteTool),
        Arc::new(crate::todos::TodoReadTool),
        // Long-lived goals with steps (objectives slice): global goals the
        // user sees with progress in the Today panel.
        Arc::new(crate::objectives::ObjectiveCreateTool),
        Arc::new(crate::objectives::ObjectiveListTool),
        Arc::new(crate::objectives::ObjectiveUpdateTool),
        Arc::new(crate::objectives::ObjectiveStepDoneTool),
        // Agent-initiated chat attachment (SPEC 6.1): hand a project file to
        // the user as a new assistant turn.
        Arc::new(crate::agent::send_file::SendFileTool),
        // Bounded undo stack (SPEC undo slice): undo the newest reversible
        // file/note/task mutation made by commands or agent tools.
        Arc::new(crate::agent::undo_tools::UndoLastTool),
        Arc::new(crate::agent::undo_tools::UndoListTool),
        // Background entity links (links slice): read what a chat/file/task
        // is connected to; links are recorded automatically by the loop.
        Arc::new(crate::links::RelatedTool),
        // Settings from chat (Q.md settings slice): the agent sees the real
        // config keys/values and edits them only after an explicit chat
        // confirmation; see settings_tools.rs for the allow/deny registry.
        Arc::new(crate::agent::settings_tools::SettingsListTool),
        Arc::new(crate::agent::settings_tools::SettingsProposeTool),
        Arc::new(crate::agent::settings_tools::SettingsApplyTool),
    ];
    for tool in tools {
        // Names are statically unique; a failure here is a programming bug.
        catalog.register(tool, true).expect("unique tool names");
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::test_harness::{setup, tool};
    use super::*;
    use crate::agent::tool::{validate_args, PermissionPolicy, ToolClass};
    use serde_json::json;
    use std::fs;

    const TOOL_NAMES: [&str; 63] = [
        "finish_task",
        "search",
        "read_file",
        "list_tree",
        "write_file",
        "edit_file",
        "apply_patch",
        "create_document",
        "create_spreadsheet",
        "ocr",
        "pdf_merge",
        "pdf_split",
        "pdf_fill_form",
        "image_transform",
        "create_folder",
        "rename_path",
        "delete_path",
        "task_list",
        "task_create",
        "task_update",
        "task_move",
        "task_delete",
        "datas_add",
        "datas_read",
        "git_status",
        "git_diff",
        "git_log",
        "git_branch",
        "git_commit",
        "git_pull",
        "git_push",
        "ssh_list_profiles",
        "ssh_exec",
        "ssh_upload",
        "ssh_download",
        "ssh_generate_key",
        "ssh_install_key",
        "ssh_save_profile_to_datas",
        "clipboard_get",
        "clipboard_set",
        "webfetch",
        "websearch",
        "http_request",
        "skill",
        "create_skill",
        "skill_exec",
        "remember",
        "recall",
        "set_user_gender",
        "forget",
        "todowrite",
        "todoread",
        "objective_create",
        "objective_list",
        "objective_update",
        "objective_step_done",
        "send_file",
        "undo_last",
        "undo_list",
        "related",
        "settings_list",
        "settings_propose",
        "settings_apply",
    ];

    #[test]
    fn catalog_registers_unique_tools_with_object_schemas() {
        let specs = project_catalog().to_specs();
        let mut names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "duplicate tool names in specs");
        names.sort();
        for expected in TOOL_NAMES {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        assert_eq!(specs.len(), TOOL_NAMES.len() + 2); // + discovery tools
        for spec in specs.iter().filter(|s| TOOL_NAMES.contains(&s.name.as_str())) {
            let p = &spec.parameters;
            assert_eq!(p["type"], "object", "{} schema type", spec.name);
            assert_eq!(p["additionalProperties"], false, "{} extras", spec.name);
            assert!(p["properties"].is_object(), "{} properties", spec.name);
            assert!(p["required"].is_array(), "{} required", spec.name);
        }
    }

    #[test]
    fn validator_accepts_good_call_and_rejects_bad_calls() {
        let schema = tool("write_file").parameters();
        assert!(validate_args(&schema, &json!({ "rel_path": "a.md", "content": "x" })).is_ok());
        let err = validate_args(&schema, &json!({ "rel_path": "a.md" })).unwrap_err();
        assert!(err.contains("content"), "{err}");
        let err = validate_args(&schema, &json!({ "rel_path": 1, "content": "x" })).unwrap_err();
        assert!(err.contains("string"), "{err}");
        let err = validate_args(&schema, &json!({ "rel_path": "a", "content": "x", "extra": 1 }))
            .unwrap_err();
        assert!(err.contains("extra"), "{err}");
    }

    #[test]
    fn policies_and_classes_match_the_spec() {
        for name in [
            "search",
            "read_file",
            "list_tree",
            "ocr",
            "task_list",
            "datas_read",
            "git_status",
            "git_diff",
            "git_log",
            "webfetch",
            "websearch",
            "skill",
            "recall",
            "todoread",
        ] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
        }
        for name in [
            "write_file",
            "edit_file",
            "create_folder",
            "task_create",
            "task_update",
            "task_move",
            "datas_add",
            "remember",
            "set_user_gender",
            "todowrite",
        ] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
        }
        for name in ["rename_path", "delete_path", "task_delete", "forget", "git_commit", "git_pull", "git_push", "apply_patch", "skill_exec", "create_skill"] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
        }
        assert_eq!(tool("git_branch").class(), ToolClass::Mutating);
        assert_eq!(tool("git_branch").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("image_transform").class(), ToolClass::Mutating);
        assert_eq!(tool("image_transform").policy(), PermissionPolicy::CheckConditional);
        let env = setup("pt-image-policy");
        let img = tool("image_transform");
        assert!(img
            .danger_reason(&json!({ "path": "a.png", "maxSize": 10 }), &env.ctx)
            .is_some());
        assert!(img
            .danger_reason(&json!({ "path": "a.png", "output": "b.png", "maxSize": 10 }), &env.ctx)
            .is_none());
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
