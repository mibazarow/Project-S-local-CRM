// Notes-agent tool set (SPEC notes chat slice): list/read/write/create notes
// in the vault, manage its folder structure (mkdir/move/copy), rename/delete
// entries and search the notes index. Every path argument is resolved strictly
// inside the Notes/ folder handed to the run through ToolCtx.notes_root; the
// tools refuse to work without it, so a project or harness run can never reach
// the vault through this catalog.
use super::catalog::Catalog;
use super::project_tools::state_of;
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::{notes, search};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

// All notes tools are core: the notes agent's whole tool set is active from
// the first round (lazy discovery stays for the harness agent, 5.2).
pub fn notes_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(crate::skills::SkillTool),
        // Global memory is shared with the project and harness agents (6.3).
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        Arc::new(NotesListTool),
        Arc::new(NotesReadTool),
        Arc::new(NotesWriteTool),
        Arc::new(NotesCreateTool),
        Arc::new(NotesMkdirTool),
        Arc::new(NotesRenameTool),
        Arc::new(NotesMoveTool),
        Arc::new(NotesCopyTool),
        Arc::new(NotesDeleteTool),
        Arc::new(SearchNotesTool),
        // Agent-initiated chat attachment (SPEC 6.1): hand a Notes file to
        // the user as a new assistant turn.
        Arc::new(crate::agent::send_file::SendFileTool),
    ];
    for tool in tools {
        // Names are statically unique; a failure here is a programming bug.
        catalog.register(tool, true).expect("unique tool names");
    }
    catalog
}

fn require_root(ctx: &ToolCtx) -> Result<PathBuf> {
    ctx.notes_root
        .clone()
        .ok_or_else(|| AppError::Invalid("no Notes vault bound to this agent run".into()))
}

// Maps a vault AppError to the envelope code (SPEC 6.4): missing entries are
// not_found, path escapes and symlink refusals are permission_denied (the
// shared join_rel helper words its refusal as "not project-relative"), name
// clashes and wrong folder targets are invalid_args, the rest (IO faults) are
// execution_error.
fn fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else if msg.contains("not Notes-relative")
        || msg.contains("not project-relative")
        || msg.contains("escapes the Notes")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else if msg.contains("already exists")
        || msg.contains("is not a folder")
        || msg.contains("invalid name")
        || msg.contains("into itself or its own subfolder")
    {
        ToolErrorCode::InvalidArgs
    } else {
        ToolErrorCode::ExecutionError
    };
    ToolResult::err(code, msg)
}

// Compact shape returned by the folder tools (mkdir/move/copy): the new entry.
fn entry_data(item: &crate::fs_ops::FsEntry) -> Value {
    json!({ "relPath": item.rel_path, "name": item.name, "kind": item.kind })
}

pub struct NotesListTool;

impl Tool for NotesListTool {
    fn name(&self) -> &str {
        "notes_list"
    }
    fn description(&self) -> &str {
        "List one folder level of the Notes vault; relPath empty lists the vault root. Returns entries with name, relPath, kind, size."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "relPath": { "type": "string" } },
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args.get("relPath").and_then(Value::as_str).unwrap_or("");
        match notes::list_at(&root, rel) {
            Ok(entries) => Ok(
                ToolResult::ok(json!({ "relPath": rel, "entries": entries }))
                    .capped(self.max_result_len()),
            ),
            Err(e) => Ok(fail(e)),
        }
    }
}

pub struct NotesReadTool;

impl Tool for NotesReadTool {
    fn name(&self) -> &str {
        "notes_read"
    }
    fn description(&self) -> &str {
        "Read a note from the Notes vault by vault-relative path. Text files only; folders and binary files are rejected."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "relPath": { "type": "string" } },
            "required": ["relPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        match notes::read_at(&root, rel) {
            Ok(content) => Ok(
                ToolResult::ok(json!({ "relPath": rel, "content": content }))
                    .capped(self.max_result_len()),
            ),
            Err(e) => Ok(fail(e)),
        }
    }
}

pub struct NotesWriteTool;

impl Tool for NotesWriteTool {
    fn name(&self) -> &str {
        "notes_write"
    }
    fn description(&self) -> &str {
        "Overwrite an existing note in the Notes vault with new content (whole file). Use notes_create for new files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relPath": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["relPath", "content"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        Ok(match notes::write_at(&root, rel, content) {
            Ok(item) => ToolResult::ok(json!({ "relPath": item.rel_path, "size": item.size })),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesCreateTool;

impl Tool for NotesCreateTool {
    fn name(&self) -> &str {
        "notes_create"
    }
    fn description(&self) -> &str {
        "Create a new note in the Notes vault; parent folders are created on demand. Fails if the path already exists."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relPath": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["relPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        let content = args.get("content").and_then(Value::as_str);
        Ok(match notes::create_at(&root, rel, content) {
            Ok(item) => ToolResult::ok(json!({ "relPath": item.rel_path, "size": item.size })),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesRenameTool;

impl Tool for NotesRenameTool {
    fn name(&self) -> &str {
        "notes_rename"
    }
    fn description(&self) -> &str {
        "Rename a note or folder inside the Notes vault (newName is a bare name, not a path)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relPath": { "type": "string" },
                "newName": { "type": "string" }
            },
            "required": ["relPath", "newName"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        let new_name = args["newName"].as_str().unwrap_or_default();
        Ok(match notes::rename_at(&root, rel, new_name) {
            Ok(item) => ToolResult::ok(json!({ "relPath": item.rel_path })),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesMkdirTool;

impl Tool for NotesMkdirTool {
    fn name(&self) -> &str {
        "notes_mkdir"
    }
    fn description(&self) -> &str {
        "Create a new folder inside the Notes vault under parentRel (empty parentRel = vault root). Fails if the name is already taken; parents are never created implicitly."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parentRel": { "type": "string" },
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let parent = args
            .get("parentRel")
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = args["name"].as_str().unwrap_or_default();
        Ok(match notes::mkdir_at(&root, parent, name) {
            Ok(item) => ToolResult::ok(entry_data(&item)),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesMoveTool;

impl Tool for NotesMoveTool {
    fn name(&self) -> &str {
        "notes_move"
    }
    fn description(&self) -> &str {
        "Move a note or a whole folder into an existing Notes folder (empty toDirRel = vault root). The target folder must exist, nothing is ever overwritten (a name clash is an error), and a folder cannot move into itself or its own subfolder."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fromRel": { "type": "string" },
                "toDirRel": { "type": "string" }
            },
            "required": ["fromRel", "toDirRel"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let from = args["fromRel"].as_str().unwrap_or_default();
        let to_dir = args["toDirRel"].as_str().unwrap_or_default();
        Ok(match notes::move_at(&root, from, to_dir) {
            Ok(item) => ToolResult::ok(entry_data(&item)),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesCopyTool;

impl Tool for NotesCopyTool {
    fn name(&self) -> &str {
        "notes_copy"
    }
    fn description(&self) -> &str {
        "Copy a note or a whole folder (recursively) into an existing Notes folder (empty toDirRel = vault root). The source stays in place; on a name clash the copy is renamed with a \" copy\" / \" copy N\" suffix."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fromRel": { "type": "string" },
                "toDirRel": { "type": "string" }
            },
            "required": ["fromRel", "toDirRel"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let from = args["fromRel"].as_str().unwrap_or_default();
        let to_dir = args["toDirRel"].as_str().unwrap_or_default();
        Ok(match notes::copy_at(&root, from, to_dir) {
            Ok(item) => ToolResult::ok(entry_data(&item)),
            Err(e) => fail(e),
        })
    }
}

pub struct NotesDeleteTool;

impl Tool for NotesDeleteTool {
    fn name(&self) -> &str {
        "notes_delete"
    }
    fn description(&self) -> &str {
        "Delete a note or folder (recursively) from the Notes vault. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "relPath": { "type": "string" } },
            "required": ["relPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let rel = args["relPath"].as_str().unwrap_or_default();
        Some(format!("удаление заметки «{rel}»"))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = require_root(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        Ok(match notes::delete_at(&root, rel) {
            Ok(()) => ToolResult::ok(json!({ "deleted": rel })),
            Err(e) => fail(e),
        })
    }
}

pub struct SearchNotesTool;

impl Tool for SearchNotesTool {
    fn name(&self) -> &str {
        "search_notes"
    }
    fn description(&self) -> &str {
        "Search the Notes vault index (FTS + semantic) and return hits with relPath, name, snippet and matchTypes."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        require_root(ctx)?;
        let state = state_of(ctx)?;
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        match search::search_notes(state, query, limit) {
            Ok(hits) => Ok(ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len())),
            Err(e) => Ok(fail(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::agent::tool::PermissionPolicy as Policy;
    use crate::db;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::state::AppState;

    fn setup(tag: &str) -> (Arc<AppState>, ToolCtx) {
        let wd = temp_test_dir(tag);
        // canonical: contain_existing compares canonical paths, and temp dirs
        // on macOS live behind the /var -> /private/var symlink.
        let vault = notes::ensure(&wd).unwrap().canonicalize().unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        let ctx = ToolCtx {
            notes_root: Some(vault),
            state: Some(Arc::clone(&state)),
            working_dir: Some(wd),
            ..Default::default()
        };
        (state, ctx)
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        let catalog = notes_catalog();
        let Resolution::Found(t) = catalog.resolve(name) else {
            panic!("tool {name} missing from notes catalog");
        };
        // A missing notes root propagates as Err from execute (run wiring
        // bug, not a model-facing envelope); surface it like the loop does.
        match t.execute(&args, ctx) {
            Ok(r) => r,
            Err(e) => ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()),
        }
    }

    #[test]
    fn catalog_carries_core_and_notes_tools_with_expected_policies() {
        let catalog = notes_catalog();
        for name in [
            "finish_task",
            "skill",
            "remember",
            "recall",
            "set_user_gender",
            "notes_list",
            "notes_read",
            "notes_write",
            "notes_create",
            "notes_mkdir",
            "notes_rename",
            "notes_move",
            "notes_copy",
            "notes_delete",
            "search_notes",
            "send_file",
        ] {
            assert!(
                matches!(catalog.resolve(name), Resolution::Found(_)),
                "missing tool: {name}"
            );
        }
        let read_only_skip = ["notes_list", "notes_read", "search_notes"];
        for name in read_only_skip {
            let t = catalog.resolve(name);
            let Resolution::Found(t) = t else { unreachable!() };
            assert_eq!(t.class(), ToolClass::ReadOnly, "{name}");
            assert_eq!(t.policy(), Policy::Skip, "{name}");
        }
        for name in [
            "notes_write",
            "notes_create",
            "notes_mkdir",
            "notes_rename",
            "notes_move",
            "notes_copy",
            "send_file",
        ] {
            let Resolution::Found(t) = catalog.resolve(name) else { unreachable!() };
            assert_eq!(t.class(), ToolClass::Mutating, "{name}");
            assert_eq!(t.policy(), Policy::Skip, "{name}");
        }
        let Resolution::Found(del) = catalog.resolve("notes_delete") else { unreachable!() };
        assert_eq!(del.policy(), Policy::Check);
        assert!(del
            .danger_reason(&json!({ "relPath": "a.md" }), &ToolCtx::default())
            .is_some());
    }

    #[test]
    fn tools_require_a_bound_notes_root() {
        let ctx = ToolCtx::default();
        for (name, args) in [
            ("notes_list", json!({})),
            ("notes_read", json!({ "relPath": "x.md" })),
            ("notes_write", json!({ "relPath": "x.md", "content": "c" })),
            ("notes_create", json!({ "relPath": "x.md" })),
            ("notes_mkdir", json!({ "name": "x" })),
            ("notes_rename", json!({ "relPath": "x.md", "newName": "y.md" })),
            ("notes_move", json!({ "fromRel": "x.md", "toDirRel": "" })),
            ("notes_copy", json!({ "fromRel": "x.md", "toDirRel": "" })),
            ("notes_delete", json!({ "relPath": "x.md" })),
            ("search_notes", json!({ "query": "q" })),
        ] {
            let r = exec(name, args, &ctx);
            assert!(!r.ok, "{name} executed without a notes root");
        }
    }

    #[test]
    fn vault_crud_roundtrip_through_tools() {
        let (_state, ctx) = setup("notes-tools-crud");
        let created = exec("notes_create", json!({ "relPath": "идея.md", "content": "раз" }), &ctx);
        assert!(created.ok, "{:?}", created.error);
        let read = exec("notes_read", json!({ "relPath": "идея.md" }), &ctx);
        assert_eq!(read.data.unwrap()["content"], "раз");
        let written = exec(
            "notes_write",
            json!({ "relPath": "идея.md", "content": "два" }),
            &ctx,
        );
        assert!(written.ok, "{:?}", written.error);
        let read = exec("notes_read", json!({ "relPath": "идея.md" }), &ctx);
        assert_eq!(read.data.unwrap()["content"], "два");
        let renamed = exec(
            "notes_rename",
            json!({ "relPath": "идея.md", "newName": "идеи.md" }),
            &ctx,
        );
        assert!(renamed.ok, "{:?}", renamed.error);
        let listed = exec("notes_list", json!({}), &ctx);
        let listed = listed.data.unwrap();
        let names: Vec<&str> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"идеи.md"));
        let deleted = exec("notes_delete", json!({ "relPath": "идеи.md" }), &ctx);
        assert!(deleted.ok, "{:?}", deleted.error);
        let missing = exec("notes_read", json!({ "relPath": "идеи.md" }), &ctx);
        assert_eq!(missing.error.unwrap().code, "not_found");
    }

    // Vault folder management through the tools: mkdir, move, copy, plus the
    // envelope codes for clashes, self-moves and bad destinations.
    #[test]
    fn folder_tools_mkdir_move_copy_roundtrip() {
        let (_state, ctx) = setup("notes-tools-folders");
        assert!(
            exec("notes_create", json!({ "relPath": "нота.md", "content": "раз" }), &ctx).ok
        );
        let dir = exec("notes_mkdir", json!({ "parentRel": "", "name": "папка" }), &ctx);
        assert!(dir.ok, "{:?}", dir.error);
        assert_eq!(dir.data.unwrap()["relPath"], "папка");
        assert_eq!(
            exec("notes_mkdir", json!({ "parentRel": "", "name": "папка" }), &ctx)
                .error
                .unwrap()
                .code,
            "invalid_args"
        );
        assert_eq!(
            exec("notes_mkdir", json!({ "name": "a/b" }), &ctx)
                .error
                .unwrap()
                .code,
            "invalid_args"
        );

        let moved = exec(
            "notes_move",
            json!({ "fromRel": "нота.md", "toDirRel": "папка" }),
            &ctx,
        );
        assert!(moved.ok, "{:?}", moved.error);
        assert_eq!(moved.data.as_ref().unwrap()["relPath"], "папка/нота.md");
        assert_eq!(moved.data.unwrap()["kind"], "file");
        // no overwrite, no move into itself
        assert_eq!(
            exec("notes_move", json!({ "fromRel": "папка", "toDirRel": "папка" }), &ctx)
                .error
                .unwrap()
                .code,
            "invalid_args"
        );
        assert_eq!(
            exec("notes_move", json!({ "fromRel": "папка/нота.md", "toDirRel": "нет" }), &ctx)
                .error
                .unwrap()
                .code,
            "not_found"
        );

        let copied = exec(
            "notes_copy",
            json!({ "fromRel": "папка/нота.md", "toDirRel": "" }),
            &ctx,
        );
        assert!(copied.ok, "{:?}", copied.error);
        assert_eq!(copied.data.unwrap()["relPath"], "нота.md");
        let again = exec(
            "notes_copy",
            json!({ "fromRel": "папка/нота.md", "toDirRel": "" }),
            &ctx,
        );
        assert_eq!(again.data.unwrap()["relPath"], "нота copy.md");
        let third = exec(
            "notes_copy",
            json!({ "fromRel": "папка/нота.md", "toDirRel": "" }),
            &ctx,
        );
        assert_eq!(third.data.unwrap()["relPath"], "нота copy 1.md");
        let dir_copy = exec("notes_copy", json!({ "fromRel": "папка", "toDirRel": "" }), &ctx);
        assert_eq!(dir_copy.data.as_ref().unwrap()["relPath"], "папка copy");
        assert_eq!(dir_copy.data.unwrap()["kind"], "dir");
        let read = exec("notes_read", json!({ "relPath": "папка copy/нота.md" }), &ctx);
        assert_eq!(read.data.unwrap()["content"], "раз");
        // the source of every copy is still in place
        assert!(
            exec("notes_read", json!({ "relPath": "папка/нота.md" }), &ctx).ok,
            "copy removed the source"
        );

        for bad in ["../x", "/etc/passwd"] {
            for (name, args) in [
                ("notes_move", json!({ "fromRel": "папка", "toDirRel": bad })),
                ("notes_copy", json!({ "fromRel": "папка", "toDirRel": bad })),
            ] {
                let r = exec(name, args, &ctx);
                assert_eq!(
                    r.error.as_ref().unwrap().code,
                    "permission_denied",
                    "{name} accepted toDirRel {bad:?}"
                );
            }
        }
    }

    #[test]
    fn path_escapes_map_to_permission_denied() {
        let (_state, ctx) = setup("notes-tools-escape");
        for bad in ["../x", "/etc/passwd", "a/../../x"] {
            for (name, args) in [
                ("notes_list", json!({ "relPath": bad })),
                ("notes_read", json!({ "relPath": bad })),
                ("notes_write", json!({ "relPath": bad, "content": "x" })),
                ("notes_create", json!({ "relPath": bad })),
                ("notes_mkdir", json!({ "parentRel": bad, "name": "x" })),
                ("notes_move", json!({ "fromRel": bad, "toDirRel": "" })),
                ("notes_copy", json!({ "fromRel": bad, "toDirRel": "" })),
                ("notes_delete", json!({ "relPath": bad })),
            ] {
                let r = exec(name, args, &ctx);
                assert!(
                    r.error.as_ref().is_some_and(|e| e.code == "permission_denied"),
                    "{name} on {bad:?}: {:?}",
                    r.error
                );
            }
        }
    }

    #[test]
    fn search_notes_tool_hits_the_vault_index() {
        let (state, ctx) = setup("notes-tools-search");
        let wd = state.working_dir().unwrap();
        notes::create(&wd, "счета/оплата.md", Some("Клиент example.ru оплатил счёт")).unwrap();
        index::index_notes(&wd).unwrap();
        let r = exec("search_notes", json!({ "query": "оплатил" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["relPath"], "счета/оплата.md");
    }
}
