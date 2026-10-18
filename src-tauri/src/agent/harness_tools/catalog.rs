// harness_catalog: the harness run's tool registry (SPEC 5.2, 6.4). The
// core (finish/skill/memory) stays active from the first round; the
// machine-wide set is registered for lazy discovery so the harness widens
// its own schema list with enable_tools. The core/deferred lists and their
// order are the contract checked by agent/invariants.rs and the catalog
// test below — keep them identical.
use super::chat_tools::{DeleteChatTool, ListChatsTool, NewChatTool, RenameChatTool, SearchChatsTool};
use super::crm_tools::{
    CrmListProjectsTool, CrmSearchTool, CrmTaskCreateTool, CrmTaskDeleteTool, CrmTaskMoveTool,
    CrmTaskUpdateTool, CrmTasksTool, DatasAddTool, DatasReadTool,
};
use super::fs_tools::{
    FsCopyTool, FsDeleteTool, FsGrepTool, FsListTool, FsMkdirTool, FsMoveTool, FsReadTool,
    FsRenameTool, FsWriteTool,
};
use super::schedule_tools::{
    ScheduleCreateTool, ScheduleDeleteTool, ScheduleListTool, SchedulePauseTool,
};
use super::shell_tools::BashTool;
use crate::agent::catalog::Catalog;
use crate::agent::tool::Tool;
use std::sync::Arc;

// Core (finish/skill/memory) stays active from the first round; the
// machine-wide set is registered for lazy discovery (SPEC 6.4) so the
// harness widens its own schema list with enable_tools.
pub fn harness_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let core: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(crate::skills::SkillTool),
        Arc::new(crate::skills::SkillExecTool),
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        Arc::new(crate::memory::ForgetTool),
        Arc::new(ListChatsTool),
        Arc::new(RenameChatTool),
        Arc::new(NewChatTool),
        Arc::new(DeleteChatTool),
        Arc::new(SearchChatsTool),
        // Per-conversation plan panel (SPEC: todowrite/todoread).
        Arc::new(crate::todos::TodoWriteTool),
        Arc::new(crate::todos::TodoReadTool),
        // Long-lived goals with steps (objectives slice): global goals the
        // user sees with progress in the Today panel.
        Arc::new(crate::objectives::ObjectiveCreateTool),
        Arc::new(crate::objectives::ObjectiveListTool),
        Arc::new(crate::objectives::ObjectiveUpdateTool),
        Arc::new(crate::objectives::ObjectiveStepDoneTool),
        // Agent-initiated chat attachment (SPEC 6.1): hand a file to the user
        // as a new assistant turn; active from the first round.
        Arc::new(super::super::send_file::SendFileTool),
    ];
    let deferred: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FsReadTool),
        Arc::new(FsListTool),
        Arc::new(FsWriteTool),
        Arc::new(FsDeleteTool),
        Arc::new(FsCopyTool),
        Arc::new(FsMoveTool),
        Arc::new(FsRenameTool),
        Arc::new(FsMkdirTool),
        Arc::new(FsGrepTool),
        // Precise edits (exact string replacement + git apply patches);
        // see edit_tools.rs for the safety rules.
        Arc::new(super::super::edit_tools::EditFileTool::harness()),
        Arc::new(super::super::edit_tools::ApplyPatchTool),
        // Native document generation (DOCX/PDF/MD/TXT/HTML + XLSX/CSV) with
        // pure Rust writers; see doc_tools.rs for the safety rules.
        Arc::new(super::super::doc_tools::CreateDocumentTool::harness()),
        Arc::new(super::super::doc_tools::CreateSpreadsheetTool::harness()),
        // OCR of images and PDFs via the macOS Vision framework through an
        // embedded Swift helper; see ocr_tools.rs for the safety rules.
        Arc::new(super::super::ocr_tools::OcrTool),
        // PDF merge/split/fill-form via Apple PDFKit through an embedded Swift
        // helper; see pdf_tools.rs for the safety rules.
        Arc::new(super::super::pdf_tools::PdfMergeTool),
        Arc::new(super::super::pdf_tools::PdfSplitTool),
        Arc::new(super::super::pdf_tools::PdfFillFormTool),
        // Image resize/crop/rotate/convert via the macOS sips CLI; in-place
        // calls confirm, new files in the territory skip; see image_tools.rs.
        Arc::new(super::super::image_tools::ImageTransformTool),
        // Screen capture via the macOS screencapture CLI; always confirmed,
        // see screenshot_tool.rs for the safety rules.
        Arc::new(super::super::screenshot_tool::ScreenshotTool::default()),
        Arc::new(crate::skills::CreateSkillTool),
        Arc::new(BashTool::default()),
        // Background process control (dev servers, installs): the background
        // twin of bash, lazy-discovered together with it; see process_tools.rs.
        Arc::new(super::super::process_tools::ProcessStartTool),
        Arc::new(super::super::process_tools::ProcessLogsTool),
        Arc::new(super::super::process_tools::ProcessStopTool),
        Arc::new(super::super::process_tools::ProcessListTool),
        Arc::new(super::super::browser_tool::BrowserTool),
        Arc::new(super::super::net_tools::WebfetchTool::new()),
        Arc::new(super::super::net_tools::WebsearchTool::new()),
        Arc::new(super::super::net_tools::HttpRequestTool),
        Arc::new(CrmListProjectsTool),
        Arc::new(CrmSearchTool),
        Arc::new(CrmTasksTool),
        Arc::new(CrmTaskCreateTool),
        Arc::new(CrmTaskUpdateTool),
        Arc::new(CrmTaskMoveTool),
        Arc::new(CrmTaskDeleteTool),
        Arc::new(DatasReadTool),
        Arc::new(DatasAddTool),
        // Git tools via the system git CLI, lazy-discovered like the rest of
        // the machine-wide set; see git_tools.rs for the safety rules.
        Arc::new(super::super::git_tools::GitStatusTool),
        Arc::new(super::super::git_tools::GitDiffTool),
        Arc::new(super::super::git_tools::GitLogTool),
        Arc::new(super::super::git_tools::GitBranchTool),
        Arc::new(super::super::git_tools::GitCommitTool),
        Arc::new(super::super::git_tools::GitPullTool),
        Arc::new(super::super::git_tools::GitPushTool),
        // SSH tools via the system ssh/scp/ssh-keygen CLI: keys, ssh-agent
        // and askpass passwords, profiles from ssh.json and the current
        // project's datas.md; lazy discovered like the rest of the
        // machine-wide set; see ssh_tools.rs.
        Arc::new(super::super::ssh_tools::SshListProfilesTool),
        Arc::new(super::super::ssh_tools::SshExecTool),
        Arc::new(super::super::ssh_tools::SshCopyTool::upload()),
        Arc::new(super::super::ssh_tools::SshCopyTool::download()),
        Arc::new(super::super::ssh_tools::SshGenerateKeyTool),
        Arc::new(super::super::ssh_tools::SshInstallKeyTool),
        Arc::new(super::super::ssh_tools::SshSaveProfileToDatasTool),
        // macOS clipboard read/write via the system pbcopy/pbpaste, lazy
        // discovered like the rest of the machine-wide set; see clipboard_tools.rs.
        Arc::new(super::super::clipboard_tools::ClipboardGetTool::default()),
        Arc::new(super::super::clipboard_tools::ClipboardSetTool::default()),
        Arc::new(ScheduleCreateTool),
        Arc::new(ScheduleListTool),
        Arc::new(SchedulePauseTool),
        Arc::new(ScheduleDeleteTool),
        Arc::new(crate::agent::undo_tools::UndoLastTool),
        Arc::new(crate::agent::undo_tools::UndoListTool),
        // Background entity links (links slice): lazy-discovered like the
        // rest of the machine-wide set.
        Arc::new(crate::links::RelatedTool),
    ];
    for tool in core {
        catalog.register(tool, true).expect("unique tool names");
    }
    for tool in deferred {
        catalog.register(tool, false).expect("unique tool names");
    }
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::{Resolution, ENABLE_TOOLS, LIST_TOOLS};
    use crate::agent::harness_tools::test_support::{setup, tool};
    use crate::agent::tool::{PermissionPolicy, ToolClass, ToolCtx};
    use serde_json::json;
    use std::fs;

    const DEFERRED_NAMES: [&str; 56] = [
        "fs_read",
        "fs_list",
        "fs_write",
        "fs_delete",
        "fs_copy",
        "fs_move",
        "fs_rename",
        "fs_mkdir",
        "fs_grep",
        "edit_file",
        "apply_patch",
        "create_document",
        "create_spreadsheet",
        "ocr",
        "pdf_merge",
        "pdf_split",
        "pdf_fill_form",
        "image_transform",
        "screenshot",
        "create_skill",
        "bash",
        "process_start",
        "process_logs",
        "process_stop",
        "process_list",
        "browser",
        "webfetch",
        "websearch",
        "http_request",
        "crm_list_projects",
        "crm_search",
        "crm_tasks",
        "crm_task_create",
        "crm_task_update",
        "crm_task_move",
        "crm_task_delete",
        "datas_read",
        "datas_add",
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
        "schedule_create",
        "schedule_list",
        "schedule_pause",
        "schedule_delete",
    ];

    #[test]
    fn catalog_registers_everything_without_duplicate_names() {
        let mut catalog = harness_catalog();
        let active: Vec<String> = catalog.to_specs().into_iter().map(|s| s.name).collect();
        for expected in ["finish_task", "skill", "skill_exec", "remember", "recall", "set_user_gender", "forget", "list_chats", "rename_chat", "new_chat", "delete_chat", "search_chats", LIST_TOOLS, ENABLE_TOOLS] {
            assert!(active.iter().any(|n| n == expected), "missing active {expected}");
        }
        let summary = catalog.list_summary();
        let available: Vec<String> = summary["available"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
        for name in DEFERRED_NAMES {
            assert!(matches!(catalog.resolve(name), Resolution::Found(_)), "missing {name}");
            assert!(available.iter().any(|n| n == name), "{name} not in available");
        }
        for name in DEFERRED_NAMES {
            assert!(matches!(catalog.enable(name), crate::agent::catalog::EnableOutcome::Activated));
        }
    }

    #[test]
    fn classes_policies_and_danger_reasons_match_the_spec() {
        for name in ["fs_read", "fs_list", "fs_grep", "ocr", "webfetch", "websearch", "crm_list_projects", "crm_search", "crm_tasks", "datas_read", "git_status", "git_diff", "git_log", "ssh_list_profiles"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["fs_write", "fs_copy", "fs_move", "fs_rename", "fs_mkdir", "crm_task_create", "crm_task_update", "crm_task_move", "datas_add", "create_skill", "ssh_save_profile_to_datas"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
        }
        assert_eq!(tool("ssh_save_profile_to_datas").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("fs_write").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_copy").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_move").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_rename").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_mkdir").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("create_skill").policy(), PermissionPolicy::Skip);
        for name in ["fs_delete", "bash", "skill_exec", "crm_task_delete", "forget", "git_commit", "git_pull", "git_push", "apply_patch", "ssh_upload", "ssh_download", "ssh_generate_key", "ssh_install_key", "screenshot"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        // ssh_exec decides per call: read-only commands skip, the rest confirm
        assert_eq!(tool("ssh_exec").class(), ToolClass::Mutating);
        assert_eq!(tool("ssh_exec").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "ls" }), &ToolCtx::default()).is_none());
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "ls | wc -l" }), &ToolCtx::default()).is_some());
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "rm -rf /" }), &ToolCtx::default()).is_some());
        // browser decides per action: read-only actions skip, the rest confirm
        assert_eq!(tool("browser").class(), ToolClass::Mutating);
        assert_eq!(tool("browser").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("browser").danger_reason(&json!({ "action": "text" }), &ToolCtx::default()).is_none());
        assert!(tool("browser").danger_reason(&json!({ "action": "open" }), &ToolCtx::default()).is_some());
        assert_eq!(tool("git_branch").class(), ToolClass::Mutating);
        assert_eq!(tool("git_branch").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("git_branch").danger_reason(&json!({ "action": "list" }), &ToolCtx::default()).is_none());
        assert!(tool("git_branch").danger_reason(&json!({ "action": "create", "name": "n" }), &ToolCtx::default()).is_some());
        for name in ["schedule_create", "schedule_delete"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({ "kind": "cron" }), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        assert_eq!(tool("schedule_list").class(), ToolClass::ReadOnly);
        assert_eq!(tool("schedule_list").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("schedule_pause").class(), ToolClass::Mutating);
        assert_eq!(tool("schedule_pause").policy(), PermissionPolicy::Skip);
        for name in ["process_logs", "process_list"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["process_start", "process_stop"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        assert_eq!(tool("list_chats").class(), ToolClass::ReadOnly);
        assert_eq!(tool("list_chats").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("rename_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("rename_chat").policy(), PermissionPolicy::Skip);
        // Chat create/rename are non-destructive and skip the queue; delete
        // destroys a session and its messages, so it confirms like other deletes.
        assert_eq!(tool("new_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("new_chat").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("delete_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("delete_chat").policy(), PermissionPolicy::Check);
        assert_eq!(tool("search_chats").class(), ToolClass::ReadOnly);
        assert_eq!(tool("search_chats").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("todowrite").class(), ToolClass::Mutating);
        assert_eq!(tool("todowrite").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("todoread").class(), ToolClass::ReadOnly);
        assert_eq!(tool("todoread").policy(), PermissionPolicy::Skip);

        let env = setup("ht-policy");
        let write = tool("fs_write");
        let inside = write.danger_reason(&json!({ "path": "Harness/a.txt", "content": "x" }), &env.ctx);
        assert_eq!(inside, None, "inside Harness must not confirm");
        let inside_projects = write.danger_reason(&json!({ "path": "Projects/p/a.txt", "content": "x" }), &env.ctx);
        assert_eq!(inside_projects, None, "inside Projects must not confirm");
        let outside = write.danger_reason(&json!({ "path": "outside.txt", "content": "x" }), &env.ctx);
        assert_eq!(outside.as_deref(), Some("запись вне Harness/ и Projects/"));
        let outside_abs = write.danger_reason(&json!({ "path": "/etc/hosts", "content": "x" }), &env.ctx);
        assert!(outside_abs.is_some());

        let edit = tool("edit_file");
        assert_eq!(edit.class(), ToolClass::Mutating);
        assert_eq!(edit.policy(), PermissionPolicy::CheckConditional);
        let edit_args = |path: &str| json!({ "path": path, "old_string": "x", "new_string": "y" });
        assert!(edit.danger_reason(&edit_args("Harness/a.txt"), &env.ctx).is_none());
        assert!(edit.danger_reason(&edit_args("Projects/p/a.txt"), &env.ctx).is_none());
        assert!(edit.danger_reason(&edit_args("outside.txt"), &env.ctx).is_some());
        assert_eq!(tool("apply_patch").class(), ToolClass::Mutating);
        assert_eq!(tool("apply_patch").policy(), PermissionPolicy::Check);

        for name in ["fs_copy", "fs_move"] {
            let t = tool(name);
            let inside = json!({ "from": "Harness/a.txt", "to": "Harness/b.txt" });
            assert!(t.danger_reason(&inside, &env.ctx).is_none(), "{name}: inside->inside must not confirm");
            let into_projects = json!({ "from": "Harness/a.txt", "to": "Projects/p/b.txt" });
            assert!(t.danger_reason(&into_projects, &env.ctx).is_none(), "{name}: inside->Projects must not confirm");
            let out_to = json!({ "from": "Harness/a.txt", "to": "outside.txt" });
            assert!(t.danger_reason(&out_to, &env.ctx).is_some(), "{name}: outside target must confirm");
            let out_from = json!({ "from": "outside.txt", "to": "Harness/b.txt" });
            assert!(t.danger_reason(&out_from, &env.ctx).is_some(), "{name}: outside source must confirm");
            let forced = json!({ "from": "Harness/a.txt", "to": "Harness/b.txt", "overwrite": true });
            assert!(t.danger_reason(&forced, &env.ctx).is_some(), "{name}: overwrite must confirm even inside");
        }

        for name in ["fs_rename", "fs_mkdir"] {
            let t = tool(name);
            assert!(
                t.danger_reason(&json!({ "path": "Harness/a.txt" }), &env.ctx)
                    .is_none(),
                "{name}: inside Harness must not confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "Projects/p/a.txt" }), &env.ctx)
                    .is_none(),
                "{name}: inside Projects must not confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "outside.txt" }), &env.ctx)
                    .is_some(),
                "{name}: outside must confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "/etc/cron.d" }), &env.ctx)
                    .is_some(),
                "{name}: absolute outside must confirm"
            );
        }

        let img = tool("image_transform");
        assert_eq!(img.class(), ToolClass::Mutating);
        assert_eq!(img.policy(), PermissionPolicy::CheckConditional);
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "maxSize": 10 }), &env.ctx).is_some());
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "output": "Harness/b.png", "maxSize": 10 }), &env.ctx).is_none());
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "output": "outside.png", "maxSize": 10 }), &env.ctx).is_some());
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
