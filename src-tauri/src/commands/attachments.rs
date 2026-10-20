use crate::{attachments, index, paths};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_notes_changed};
use tauri::{AppHandle, State};

// Chat attachments (SPEC 6.1): copy files into the project's `Входящие/` (or
// `Harness/Входящие/` for a harness session, `Notes/Входящие/` for the notes
// chat), journal project copies in INBOX.md, and index them so they are
// searchable. Exactly one of projectId / sessionId / notes selects the target.
#[tauri::command]
pub(crate) fn attach_files(
    app: AppHandle,
    state: State<AppState>,
    project_id: Option<i64>,
    session_id: Option<i64>,
    notes: Option<bool>,
    paths: Vec<String>,
) -> Result<Vec<attachments::AttachmentDto>> {
    let wd = state.working_dir()?;
    // Finder drops may carry folders and vanished paths: silently keep only
    // real files so a mixed drop never fails the send or journals junk.
    let paths: Vec<String> = paths
        .into_iter()
        .filter(|p| std::path::Path::new(p).is_file())
        .collect();
    match (project_id, session_id, notes.unwrap_or(false)) {
        (None, None, true) => {
            let items = attachments::attach_notes(&wd, &paths)?;
            for item in &items {
                emit_notes_changed(&app, format!("{}/{}", paths::PROJECT_INBOX, item.name));
            }
            Ok(items)
        }
        (Some(pid), None, false) => {
            let items = with_db(&state, |conn| {
                attachments::attach_project(conn, &wd, pid, &paths)
            })?;
            // Best-effort: make the copies searchable now; the watcher reindexes
            // again on the fs-changed event.
            let _ = index::index_project(&state, pid);
            emit_fs_changed(&app, pid, paths::PROJECT_INBOX);
            Ok(items)
        }
        (None, Some(sid), false) => with_db(&state, |conn| {
            attachments::attach_harness(conn, &wd, sid, &paths)
        }),
        _ => Err(AppError::Invalid(
            "exactly one of projectId, sessionId or notes is required".into(),
        )),
    }
}

// Reads an attachment image as a base64 data URL for chat thumbnails.
// savedPath is project-relative for project attachments and absolute for
// harness ones (and for pending source paths before saving); the resolver in
// attachments.rs handles both.
#[tauri::command]
pub(crate) fn attachment_data_url(
    state: State<AppState>,
    project_id: Option<i64>,
    saved_path: String,
    max_bytes: Option<u64>,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::attachment_data_url(
            conn,
            &wd,
            project_id,
            &saved_path,
            max_bytes.unwrap_or(20 * 1024 * 1024),
        )
    })
}

// Reads one attachment image of a chat message as a base64 data URL, for the
// chat UI to render agent-sent files (SPEC 6.1). The savedPath is resolved by
// the chat's kind (project / notes / harness). Images only; index selects the
// Nth attachment of the message (0-based).
#[tauri::command]
pub(crate) fn chat_attachment_data_url(
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
    index: usize,
    max_bytes: Option<u64>,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::chat_attachment_data_url(
            conn,
            &wd,
            chat_id,
            message_id,
            index,
            max_bytes.unwrap_or(20 * 1024 * 1024),
        )
    })
}

// Opens one attachment (document or image) of a chat message with the macOS
// default handler, for the chat UI to open agent-sent files (SPEC 6.1). The
// savedPath is resolved by the chat's kind (project / notes / harness); the
// attachment must belong to that chat/message or the call is refused.
#[tauri::command]
pub(crate) fn chat_attachment_open(
    app: AppHandle,
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
    index: usize,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::chat_attachment_open(&app, conn, &wd, chat_id, message_id, index)
    })
}

// Opens an attachment (document or image) with the macOS default handler.
#[tauri::command]
pub(crate) fn attachment_open(
    app: AppHandle,
    state: State<AppState>,
    project_id: Option<i64>,
    saved_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::open_attachment(&app, conn, &wd, project_id, &saved_path)
    })
}
