// Agent-initiated chat attachments (SPEC 6.1): the agent hands an existing
// file in its own scope to the user as an attachment of a fresh assistant turn.
// One tool serves all three run kinds; the scope is read from ToolCtx:
//   notes run   -> path is Notes-vault-relative, savedPath stays vault-relative
//   project run -> path is project-relative, savedPath stays project-relative
//   harness run -> path is absolute (or working-dir-relative), savedPath absolute
// The file is never copied: it already lives inside the run's allowed scope, so
// the persisted savedPath is exactly how the UI (chat_attachment_data_url) and
// the history DTO resolve it back. The assistant message is appended through
// chat::append_message so it lands in the transcript and the DB copy in the
// same order the model saw it, and an "agent-file" event lets an open chat
// render the attachment without a history reload.
use super::result::{ToolErrorCode, ToolResult};
use crate::sync_util::lock_recover;
use super::tool::{PermissionPolicy, Tool, ToolCtx};
use crate::attachments::{self, AttachmentRef};
use crate::chat;
use crate::error::{AppError, Result};
use crate::extract;
use crate::fs_ops;
use crate::llm::Role;
use crate::projects;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct SendFileTool;

impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "send_file"
    }
    fn description(&self) -> &str {
        "Attach an existing file from the current scope to this chat as an agent-sent file (a new assistant turn carrying it). Path is project-relative in a project run, Notes-vault-relative in a notes run, absolute (or working-dir-relative) in a harness run. Optional caption is the message text shown with the file. The file is not copied; it must already exist in the scope."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Existing file path in the run's scope (project-/Notes-relative, or absolute in a harness run)"
                },
                "caption": {
                    "type": "string",
                    "description": "Optional text for the assistant message that carries the attachment"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    // The file already exists in the run's own scope and only a new chat turn
    // is written, so this is a mutating but non-destructive action: no prompt.
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }

    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let raw = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "path is required")),
        };
        let caption = args.get("caption").and_then(Value::as_str).unwrap_or("");
        let chat_id = match ctx.chat_id {
            Some(id) => id,
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    "no chat bound to this agent run",
                ))
            }
        };
        let state = match ctx.state.as_deref() {
            Some(s) => s,
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    "agent state unavailable",
                ))
            }
        };

        // Resolve the file inside the run's scope and the savedPath to persist.
        let (abs, saved_path) = {
            let guard = lock_recover(&state.db);
            let conn = match guard.as_ref() {
                Some(c) => c,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::ExecutionError,
                        "database not configured",
                    ))
                }
            };
            match resolve_send_path(ctx, conn, raw) {
                Ok(v) => v,
                Err(e) => return Ok(fail(e)),
            }
        };

        let name = abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = abs
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let is_image = fs_ops::IMAGE_EXTS.contains(&ext.as_str());
        let kind: &'static str = if is_image {
            "image"
        } else if extract::is_supported(&abs) {
            "document"
        } else {
            "other"
        };

        // Append the assistant turn, then link the saved file to it.
        let message_id = {
            let guard = lock_recover(&state.db);
            let conn = match guard.as_ref() {
                Some(c) => c,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::ExecutionError,
                        "database not configured",
                    ))
                }
            };
            let msg = match chat::append_message(
                conn,
                chat_id,
                Role::Assistant,
                Some(caption),
                &[],
                None,
                None,
            ) {
                Ok(m) => m,
                Err(e) => return Ok(fail(e)),
            };
            let reference = AttachmentRef {
                name: name.clone(),
                saved_path: saved_path.clone(),
                kind: kind.to_string(),
                is_image,
            };
            if let Err(e) = attachments::save_message_attachments(conn, msg.id, &[reference]) {
                return Ok(fail(e));
            }
            msg.id
        };

        // Tell an open chat UI about the new agent file so it renders at once.
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event(
                "agent-file",
                json!({
                    "chatId": chat_id,
                    "messageId": message_id,
                    "name": name,
                    "path": saved_path,
                    "kind": kind,
                    "isImage": is_image,
                    "caption": caption,
                }),
            );
        }

        Ok(ToolResult::ok(json!({
            "sent": true,
            "messageId": message_id,
            "name": name,
            "kind": kind,
            "path": saved_path,
        })))
    }
}

// Resolves the requested path within the run's scope and returns the canonical
// absolute file plus the savedPath to persist (scope-relative for project/notes,
// absolute for harness). Directories and missing paths are rejected.
fn resolve_send_path(ctx: &ToolCtx, conn: &Connection, raw: &str) -> Result<(PathBuf, String)> {
    if let Some(root) = ctx.notes_root.as_deref() {
        let abs = fs_ops::contain_existing(root, raw, "Notes")?;
        if abs.is_dir() {
            return Err(AppError::Invalid(format!("\"{raw}\" is a folder")));
        }
        return Ok((abs, raw.to_string()));
    }
    if let Some(pid) = ctx.project_id {
        let wd = ctx.working_dir.as_deref().ok_or(AppError::NotConfigured)?;
        let abs = fs_ops::resolve_existing(conn, wd, pid, raw)?;
        let root = projects::project_root(conn, wd, pid)?;
        let saved = abs
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| abs.display().to_string());
        return Ok((abs, saved));
    }
    let wd = ctx.working_dir.as_deref().ok_or(AppError::NotConfigured)?;
    let joined = crate::agent::harness_tools::resolve_path(raw, wd);
    let canon = joined
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))?;
    if canon.is_dir() {
        return Err(AppError::Invalid(format!("\"{raw}\" is a folder")));
    }
    let saved = canon.display().to_string();
    Ok((canon, saved))
}

// Maps a domain AppError to the envelope code (SPEC 6.4): missing files are
// not_found, a folder or non-file is invalid_args, path escapes and symlink
// refusals are permission_denied, the rest are execution_error.
fn fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else if msg.contains("is a folder") || msg.contains("not a file") {
        ToolErrorCode::InvalidArgs
    } else if msg.contains("escapes")
        || msg.contains("not project-relative")
        || msg.contains("not Notes-relative")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else if msg.contains("ambiguous") {
        ToolErrorCode::InvalidArgs
    } else {
        ToolErrorCode::ExecutionError
    };
    ToolResult::err(code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::ToolCtx;
    use crate::chat;
    use crate::db;
    use crate::llm;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use crate::state::AppState;
    use std::fs;
    use std::sync::Arc;

    // Working dir + DB + one project, mirroring the attachments tests.
    fn setup(tag: &str) -> (PathBuf, Arc<AppState>, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        drop(conn);
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        (wd, Arc::new(state), dto.id)
    }

    fn project_ctx(state: &Arc<AppState>, wd: &std::path::Path, pid: i64, chat_id: i64) -> ToolCtx {
        ToolCtx {
            project_id: Some(pid),
            session_id: None,
            chat_id: Some(chat_id),
            working_dir: Some(wd.to_path_buf()),
            notes_root: None,
            state: Some(Arc::clone(state)),
        }
    }

    #[test]
    fn project_send_file_appends_assistant_turn_with_attachment() {
        let (wd, state, pid) = setup("send-proj");
        let conn = state.db.lock().unwrap();
        let chat_id = chat::ensure_project_chat(conn.as_ref().unwrap(), pid).unwrap();
        // A real file inside the project scope.
        let root = paths::projects_dir(&wd).join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("отчёт.md"), "# отчёт\nданные").unwrap();
        drop(conn);

        let ctx = project_ctx(&state, &wd, pid, chat_id);
        let r = SendFileTool
            .execute(&json!({ "path": "отчёт.md", "caption": "вот отчёт" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["kind"], "document");
        assert_eq!(r.data.as_ref().unwrap()["path"], "отчёт.md");
        let message_id = r.data.as_ref().unwrap()["messageId"].as_i64().unwrap();

        // The assistant turn carries the attachment and shows up in history.
        let conn = state.db.lock().unwrap();
        let history = chat::load_history(conn.as_ref().unwrap(), chat_id, false).unwrap();
        let last = history.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(last.text_content(), Some("вот отчёт"));
        let stored = chat::chat_history(conn.as_ref().unwrap(), pid).unwrap();
        let msg = stored.iter().find(|m| m.id == message_id).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].name, "отчёт.md");
        assert_eq!(msg.attachments[0].saved_path, "отчёт.md");
        assert_eq!(msg.attachments[0].kind, "document");
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn notes_send_file_uses_vault_relative_saved_path() {
        let wd = temp_test_dir("send-notes");
        let vault = paths::notes_dir(&wd);
        fs::create_dir_all(&vault).unwrap();
        fs::write(vault.join("идея.md"), "# идея").unwrap();
        let vault = vault.canonicalize().unwrap();

        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        let conn = state.db.lock().unwrap();
        let chat_id = crate::notes::ensure_notes_chat(conn.as_ref().unwrap()).unwrap();
        drop(conn);

        let ctx = ToolCtx {
            project_id: None,
            session_id: None,
            chat_id: Some(chat_id),
            working_dir: Some(wd.clone()),
            notes_root: Some(vault),
            state: Some(Arc::new(state)),
        };
        let r = SendFileTool
            .execute(&json!({ "path": "идея.md" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["path"], "идея.md");
        assert_eq!(r.data.as_ref().unwrap()["kind"], "document");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn harness_send_file_persists_absolute_path() {
        let (wd, state, _pid) = setup("send-harness");
        let conn = state.db.lock().unwrap();
        let session = crate::harness::create_session(conn.as_ref().unwrap(), Some("s")).unwrap();
        let chat_id = crate::harness::session_chat_id(conn.as_ref().unwrap(), session.id).unwrap();
        drop(conn);
        let file = wd.join("log.txt");
        fs::write(&file, "строчка").unwrap();

        let ctx = ToolCtx {
            project_id: None,
            session_id: Some(session.id),
            chat_id: Some(chat_id),
            working_dir: Some(wd.clone()),
            notes_root: None,
            state: Some(Arc::clone(&state)),
        };
        let r = SendFileTool
            .execute(&json!({ "path": file.display().to_string() }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let saved = r.data.as_ref().unwrap()["path"].as_str().unwrap();
        assert!(std::path::Path::new(saved).is_absolute());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_and_folder_paths_are_rejected() {
        let (wd, state, pid) = setup("send-bad");
        let conn = state.db.lock().unwrap();
        let chat_id = chat::ensure_project_chat(conn.as_ref().unwrap(), pid).unwrap();
        let root = paths::projects_dir(&wd).join("proj");
        fs::create_dir_all(root.join("папка")).unwrap();
        drop(conn);

        let ctx = project_ctx(&state, &wd, pid, chat_id);
        let missing = SendFileTool
            .execute(&json!({ "path": "нет.md" }), &ctx)
            .unwrap();
        assert_eq!(missing.error.as_ref().unwrap().code, "not_found");
        let folder = SendFileTool
            .execute(&json!({ "path": "папка" }), &ctx)
            .unwrap();
        assert_eq!(folder.error.as_ref().unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn image_attachment_is_flagged_as_image() {
        let (wd, state, pid) = setup("send-image");
        let conn = state.db.lock().unwrap();
        let chat_id = chat::ensure_project_chat(conn.as_ref().unwrap(), pid).unwrap();
        let root = paths::projects_dir(&wd).join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("pic.png"), llm::TINY_PNG).unwrap();
        drop(conn);

        let ctx = project_ctx(&state, &wd, pid, chat_id);
        let r = SendFileTool
            .execute(&json!({ "path": "pic.png" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["kind"], "image");
        fs::remove_dir_all(&wd).unwrap();
    }
}
