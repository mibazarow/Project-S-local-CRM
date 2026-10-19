// Harness tool set (SPEC 5.2 items 1, 2, 4, 5): machine-wide fs tools, a bash
// tool, CRM tools, and the scheduler tools. Unlike the project tools, harness
// paths are absolute, ~-expanded, or relative to the working dir; the default
// territory is Harness/. fs_write skips confirmation only inside Harness/ and
// Projects/ (CheckConditional), and so do fs_copy/fs_move unless overwrite is
// requested or an endpoint lies outside, and fs_rename/fs_mkdir when the path
// lies outside; fs_delete and bash are always
// confirmed — the
// confirmation is the guard, there is no path allowlist. Child processes of
// bash always see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2), never
// the system /tmp. The CDP browser tool lives in browser_tool.rs.
// (S3.2) The set is split by concern into submodules: fs_tools (fs_read/
// list/grep/write/delete/copy/move/rename/mkdir), shell_tools (bash),
// crm_tools (crm_* + datas), schedule_tools (schedule_*), chat_tools
// (chat/session tools), catalog (harness_catalog with the core/deferred
// lists) and transfer (the fs_copy/fs_move body). The shared path/policy
// primitives stay here so the external import paths (super::harness_tools::…)
// keep working unchanged.
mod catalog;
mod chat_tools;
mod crm_tools;
mod fs_tools;
mod mcp_tools;
mod schedule_tools;
mod shell_tools;
mod transfer;

pub use self::catalog::harness_catalog;

use super::project_tools::envelope;
use super::result::{ToolErrorCode, ToolResult};
use crate::error::{AppError, Result};
use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};

// Shared by fs_read and the project read_file: images are not text and must
// not be base64-inlined into the tool result (SPEC: attach in chat instead).
pub(crate) const IMAGE_READ_HINT: &str = "Image files cannot be read as text; attach the image in the chat so a vision-capable model can see it.";

// Expands "~", joins relative paths onto the working dir, then collapses
// "." and ".." lexically (the target may not exist yet, e.g. fs_write);
// canonicalish is the best-effort canonical form for policy checks. Both are
// the shared path_policy primitives (S1.6), re-exported for the other agent
// tool modules.
pub(crate) use crate::path_policy::{canonicalish, normalize, resolve_path};

// SPEC 5.2: writes inside Harness/ and Projects/ skip confirmation.
pub(crate) fn in_default_territory(path: &Path, wd: &Path) -> bool {
    let base = wd.canonicalize().unwrap_or_else(|_| normalize(wd));
    let resolved = canonicalish(path);
    resolved.starts_with(base.join(paths::HARNESS_DIR))
        || resolved.starts_with(base.join(paths::PROJECTS_DIR))
}

// Lenient re-anchoring for paths that must already exist: an exact hit
// passes through untouched; a miss is re-resolved component by component
// against the real folder entries (NFC/NFD, case, dash and space variants),
// returning the ACTUAL on-disk path. Ambiguous matches and misses surface
// as AppError (see lenient_fail).
pub(crate) fn lenient_existing(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(path.to_path_buf());
    }
    crate::path_policy::lenient_absolute(path)
}

// Maps a lenient-resolution failure to the envelope: ambiguous candidates
// are invalid_args (the caller must pick one), plain misses keep not_found.
pub(crate) fn lenient_fail(e: &AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("ambiguous") {
        ToolErrorCode::InvalidArgs
    } else {
        ToolErrorCode::NotFound
    };
    ToolResult::err(code, msg)
}

// CRM domain errors map through the same envelope as the project tools.
pub(crate) fn envelope_r(e: AppError) -> ToolResult {
    envelope::<()>(Err(e))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::harness_catalog;
    use crate::agent::catalog::Resolution;
    use crate::agent::result::ToolResult;
    use crate::agent::tool::{Tool, ToolCtx};
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::state::{AppState, UiEmitter};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub(crate) struct RecordingEmitter {
        pub(crate) events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        }
    }

    pub(crate) struct Env {
        pub(crate) wd: PathBuf,
        pub(crate) ctx: ToolCtx,
        pub(crate) events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    pub(crate) fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let recorder = RecordingEmitter::default();
        let events = recorder.events.clone();
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        let ctx = ToolCtx {
            project_id: None,
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        Env { wd, ctx, events }
    }

    pub(crate) fn tool(name: &str) -> Arc<dyn Tool> {
        let catalog = harness_catalog();
        match catalog.resolve(name) {
            Resolution::Found(t) => t.clone(),
            _ => panic!("tool {name} missing from harness catalog"),
        }
    }

    pub(crate) fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        tool(name).execute(&args, ctx).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn resolve_path_expands_home_absolute_and_relative() {
        let wd = PathBuf::from("/wd");
        assert_eq!(resolve_path("a/b", &wd), PathBuf::from("/wd/a/b"));
        assert_eq!(resolve_path("./a/../b", &wd), PathBuf::from("/wd/b"));
        assert_eq!(resolve_path("/x/y", &wd), PathBuf::from("/x/y"));
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(resolve_path("~/notes.md", &wd), home.join("notes.md"));
        assert_eq!(resolve_path("~", &wd), home);
    }
}
