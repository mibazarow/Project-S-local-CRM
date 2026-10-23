// Shared fs_copy/fs_move body (S3.2): decomposed from one long function into
// three named steps with the SAME order and semantics as before —
// plan_transfer resolves both paths, refuses symlinks and directory-into-
// itself, applies the collision rules and creates the target parents;
// execute_transfer performs the rename (with the cross-volume copy+delete
// fallback) or the copy; record_transfer_undo writes the undo/version
// bookkeeping. fs_transfer keeps the original signature and result envelope.
use crate::agent::project_tools::{emit_versions_changed, require_wd, with_conn};
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::ToolCtx;
use super::{canonicalish, lenient_existing, lenient_fail, resolve_path};
use crate::error::Result;
use crate::undo;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

// EXDEV (cross-device link): errno 18 on both macOS and Linux; rename()
// fails with it when from/to live on different volumes, and fs_move then
// falls back to copy + remove.
const EXDEV: i32 = 18;

// Symlink anywhere under the working dir, or the final entry of a path
// outside it: fs_copy/fs_move never follow or replace symlinks. The chain
// walk below the working dir is the shared path_policy guard (S1.6).
fn symlink_guard(path: &Path, wd: &Path) -> Option<PathBuf> {
    crate::path_policy::first_symlink_below(wd, path).or_else(|| {
        crate::path_policy::is_symlink(path).then(|| path.to_path_buf())
    })
}

// Recursive copy; symlinks inside the tree are skipped, never followed.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<u64> {
    fs::create_dir_all(to)?;
    let mut bytes = 0u64;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for item in fs::read_dir(&src_dir)? {
            let item = item?;
            let ft = item.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let dst = dst_dir.join(item.file_name());
            if ft.is_dir() {
                fs::create_dir_all(&dst)?;
                stack.push((item.path(), dst));
            } else {
                bytes += fs::copy(item.path(), &dst)?;
            }
        }
    }
    Ok(bytes)
}

// Total size of a file or tree (symlinks not followed), for the bytes field.
fn entry_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            if let Ok(ft) = item.file_type() {
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    stack.push(item.path());
                } else if let Ok(m) = item.metadata() {
                    total += m.len();
                }
            }
        }
    }
    total
}

// Everything execute_transfer and record_transfer_undo need, decided by the
// plan step in the original order.
pub(crate) struct TransferPlan {
    from: PathBuf,
    target: PathBuf,
    is_dir: bool,
    wd: PathBuf,
    target_existed: bool,
    target_previous: Option<String>,
}

// The plan step either produces a ready plan or a refusal envelope to be
// returned as-is (the same ToolResult::err the original body returned at
// that point).
enum Planned {
    Ready(TransferPlan),
    Refused(ToolResult),
}

fn plan_transfer(args: &Value, ctx: &ToolCtx, verb: &str) -> Result<Planned> {
    let wd = require_wd(ctx)?;
    let from_raw = args["from"].as_str().unwrap_or_default();
    let to_raw = args["to"].as_str().unwrap_or_default();
    if from_raw.is_empty() || to_raw.is_empty() {
        return Ok(Planned::Refused(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            "from and to are required",
        )));
    }
    let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    let from = resolve_path(from_raw, &wd);
    let to = resolve_path(to_raw, &wd);
    let from = match lenient_existing(&from) {
        Ok(p) => p,
        Err(e) => return Ok(Planned::Refused(lenient_fail(&e))),
    };
    // An existing destination (also via a lenient variant match) keeps its
    // real name for the collision rules; a genuinely new one keeps the
    // requested name. An ambiguous destination is refused, never guessed.
    let to = match lenient_existing(&to) {
        Ok(p) => p,
        Err(e) if e.to_string().contains("ambiguous") => {
            return Ok(Planned::Refused(lenient_fail(&e)))
        }
        Err(_) => to,
    };
    for path in [&from, &to] {
        if let Some(link) = symlink_guard(path, &wd) {
            return Ok(Planned::Refused(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} is a symlink; {verb} does not follow symlinks", link.display()),
            )));
        }
    }
    let meta = match fs::symlink_metadata(&from) {
        Ok(m) => m,
        Err(e) => return Ok(Planned::Refused(io_fail(&from, &e))),
    };
    let is_dir = meta.is_dir();
    // An existing directory `to` means "transfer into it", keeping the name.
    let target = if fs::metadata(&to).map(|m| m.is_dir()).unwrap_or(false) {
        match from.file_name() {
            Some(name) => to.join(name),
            None => {
                return Ok(Planned::Refused(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} has no name to {verb}", from.display()),
                )))
            }
        }
    } else {
        to.clone()
    };
    if let Some(link) = symlink_guard(&target, &wd) {
        return Ok(Planned::Refused(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!("{} is a symlink; {verb} does not follow symlinks", link.display()),
        )));
    }
    if is_dir && canonicalish(&target).starts_with(canonicalish(&from)) {
        return Ok(Planned::Refused(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "cannot {verb} a directory into itself or its own subfolder: {}",
                target.display()
            ),
        )));
    }
    if let Ok(tmeta) = fs::symlink_metadata(&target) {
        if !overwrite {
            return Ok(Planned::Refused(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "destination exists: {}; pass overwrite=true to replace a file",
                    target.display()
                ),
            )));
        }
        if is_dir || tmeta.is_dir() {
            return Ok(Planned::Refused(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "overwrite only replaces files, not directories; remove {} with fs_delete first",
                    target.display()
                ),
            )));
        }
    }
    let target_existed = fs::symlink_metadata(&target).is_ok();
    let target_previous = if target_existed && !is_dir {
        fs::read_to_string(&target).ok()
    } else {
        None
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return Ok(Planned::Refused(io_fail(parent, &e)));
        }
    }
    Ok(Planned::Ready(TransferPlan {
        from,
        target,
        is_dir,
        wd,
        target_existed,
        target_previous,
    }))
}

fn execute_transfer(plan: &TransferPlan, move_it: bool) -> std::result::Result<u64, ToolResult> {
    if move_it {
        let total = entry_bytes(&plan.from);
        match fs::rename(&plan.from, &plan.target) {
            Ok(()) => Ok(total),
            Err(e) if e.raw_os_error() == Some(EXDEV) => {
                let copied = if plan.is_dir {
                    copy_tree(&plan.from, &plan.target)
                } else {
                    fs::copy(&plan.from, &plan.target)
                };
                match copied {
                    Ok(n) => {
                        let removed = if plan.is_dir {
                            fs::remove_dir_all(&plan.from)
                        } else {
                            fs::remove_file(&plan.from)
                        };
                        if let Err(e) = removed {
                            return Err(ToolResult::err(
                                ToolErrorCode::ExecutionError,
                                format!(
                                    "copied {} to {} but could not remove the source: {e}",
                                    plan.from.display(),
                                    plan.target.display()
                                ),
                            ));
                        }
                        Ok(n)
                    }
                    Err(e) => Err(io_fail(&plan.target, &e)),
                }
            }
            Err(e) => Err(io_fail(&plan.from, &e)),
        }
    } else if plan.is_dir {
        match copy_tree(&plan.from, &plan.target) {
            Ok(n) => Ok(n),
            Err(e) => Err(io_fail(&plan.target, &e)),
        }
    } else {
        match fs::copy(&plan.from, &plan.target) {
            Ok(n) => Ok(n),
            Err(e) => Err(io_fail(&plan.target, &e)),
        }
    }
}

fn record_transfer_undo(plan: &TransferPlan, move_it: bool, ctx: &ToolCtx) {
    if move_it {
        let _ = with_conn(ctx, |conn| {
            undo::push_rename(
                conn,
                "harness",
                &plan.from.display().to_string(),
                &plan.target.display().to_string(),
                None,
                None,
            )
        });
    } else if let Some(prev) = &plan.target_previous {
        let abs = plan.target.display().to_string();
        let version_id = with_conn(ctx, |conn| {
            crate::reliability::version_save(conn, &plan.wd, "harness", &abs, prev)
        })
        .ok();
        if version_id.is_some() {
            emit_versions_changed(ctx, "harness", &abs);
            let _ = with_conn(ctx, |conn| {
                undo::push_file_write(conn, "harness", &abs, version_id, false, None, None)
            });
        }
    } else if !plan.target_existed {
        let _ = with_conn(ctx, |conn| {
            undo::push_file_write(
                conn,
                "harness",
                &plan.target.display().to_string(),
                None,
                true,
                None,
                None,
            )
        });
    }
}

pub(crate) fn fs_transfer(args: &Value, ctx: &ToolCtx, move_it: bool, max_len: usize) -> Result<ToolResult> {
    let verb = if move_it { "move" } else { "copy" };
    let plan = match plan_transfer(args, ctx, verb)? {
        Planned::Ready(plan) => plan,
        Planned::Refused(result) => return Ok(result),
    };
    let bytes = match execute_transfer(&plan, move_it) {
        Ok(bytes) => bytes,
        Err(result) => return Ok(result),
    };
    record_transfer_undo(&plan, move_it, ctx);
    Ok(ToolResult::ok(json!({
        "from": plan.from.display().to_string(),
        "to": plan.target.display().to_string(),
        "kind": if plan.is_dir { "dir" } else { "file" },
        "bytes": bytes,
    }))
    .capped(max_len))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use crate::agent::harness_tools::test_support::{exec, setup, tool};
    #[cfg(not(unix))]
    use crate::agent::harness_tools::test_support::{exec, setup};
    use serde_json::json;
    use std::fs;

    // Focused test for the plan step's entry guard: empty/missing from or to
    // is refused before any path resolution or filesystem touch.
    #[test]
    fn fs_transfer_refuses_empty_from_or_to_before_touching_the_fs() {
        let env = setup("ht-transfer-empty");
        for args in [
            json!({ "from": "", "to": "Harness/x.txt" }),
            json!({ "from": "Harness/a.txt", "to": "" }),
            json!({ "from": "Harness/a.txt" }),
            json!({ "to": "Harness/x.txt" }),
        ] {
            for name in ["fs_copy", "fs_move"] {
                let r = exec(name, args.clone(), &env.ctx);
                let err = r.error.unwrap();
                assert_eq!(err.code, "invalid_args", "{name} {args}");
                assert_eq!(err.message, "from and to are required", "{name} {args}");
            }
        }
        assert!(!env.wd.join("Harness/x.txt").exists());
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_copy_and_move_files_dirs_collisions_and_parents() {
        let env = setup("ht-copy");
        let h = env.wd.join("Harness");

        // copy a file: the source stays, kind/bytes are reported
        fs::write(h.join("src.txt"), "hello").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dst.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["kind"], "file");
        assert_eq!(data["bytes"], 5);
        assert_eq!(data["to"], h.join("dst.txt").display().to_string());
        assert_eq!(fs::read_to_string(h.join("dst.txt")).unwrap(), "hello");
        assert!(h.join("src.txt").is_file(), "copy must keep the source");

        // copy into an existing directory keeps the source name
        fs::create_dir_all(h.join("dir")).unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["to"], h.join("dir/src.txt").display().to_string());
        assert!(h.join("dir/src.txt").is_file());

        // collision without overwrite
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dir" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("destination exists"), "{}", err.message);

        // overwrite=true replaces the file
        fs::write(h.join("dst.txt"), "old content").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dst.txt", "overwrite": true }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(h.join("dst.txt")).unwrap(), "hello");

        // missing parents of the target are created
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/deep/nested/copy.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(h.join("deep/nested/copy.txt")).unwrap(), "hello");

        // recursive directory copy with byte totals
        fs::create_dir_all(h.join("tree/sub")).unwrap();
        fs::write(h.join("tree/a.txt"), "aaa").unwrap();
        fs::write(h.join("tree/sub/b.txt"), "bbbb").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": "Harness/tree2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["kind"], "dir");
        assert_eq!(data["bytes"], 7);
        assert_eq!(fs::read_to_string(h.join("tree2/sub/b.txt")).unwrap(), "bbbb");

        // a directory cannot be copied into itself or its own subfolder
        for to in ["Harness/tree", "Harness/tree/sub"] {
            let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": to }), &env.ctx);
            let err = r.error.unwrap();
            assert_eq!(err.code, "invalid_args", "{to}");
            assert!(err.message.contains("itself"), "{}", err.message);
        }

        // an existing directory destination is never merged or overwritten
        let r = exec("fs_copy", json!({ "from": "Harness/tree/sub", "to": "Harness/tree2", "overwrite": true }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("directories") && err.message.contains("fs_delete"), "{}", err.message);

        // move a file: the source is gone
        let r = exec("fs_move", json!({ "from": "Harness/dst.txt", "to": "Harness/moved.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["bytes"], 5);
        assert!(!h.join("dst.txt").exists());
        assert_eq!(fs::read_to_string(h.join("moved.txt")).unwrap(), "hello");

        // move a directory
        let r = exec("fs_move", json!({ "from": "Harness/tree2", "to": "Harness/tree3" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["kind"], "dir");
        assert_eq!(fs::read_to_string(h.join("tree3/a.txt")).unwrap(), "aaa");
        assert!(!h.join("tree2").exists());

        // move collision, then overwrite
        let r = exec("fs_move", json!({ "from": "Harness/moved.txt", "to": "Harness/src.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");
        let r = exec("fs_move", json!({ "from": "Harness/moved.txt", "to": "Harness/src.txt", "overwrite": true }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!h.join("moved.txt").exists());
        assert_eq!(fs::read_to_string(h.join("src.txt")).unwrap(), "hello");

        // non-existent source
        let r = exec("fs_copy", json!({ "from": "Harness/missing.txt", "to": "Harness/x.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = exec("fs_move", json!({ "from": "Harness/missing.txt", "to": "Harness/x.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fs_copy_and_move_never_follow_symlinks_and_skip_links_in_trees() {
        let env = setup("ht-copy-symlink");
        let h = env.wd.join("Harness");
        let outside = env.wd.join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, h.join("link")).unwrap();

        // a territory link resolving outside confirms (policy sees the real
        // path) and execution refuses to follow it
        let copy = tool("fs_copy");
        let args = json!({ "from": "Harness/link/x.txt", "to": "Harness/y.txt" });
        assert!(copy.danger_reason(&args, &env.ctx).is_some(), "symlink escape must confirm");
        let r = exec("fs_copy", args, &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("symlink"), "{}", err.message);
        assert!(!h.join("y.txt").exists());

        // symlinks inside a copied tree are skipped, not followed
        fs::create_dir_all(h.join("tree")).unwrap();
        fs::write(h.join("tree/real.txt"), "r").unwrap();
        std::os::unix::fs::symlink(h.join("tree/real.txt"), h.join("tree/lnk.txt")).unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": "Harness/tree-copy" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["bytes"], 1);
        assert!(h.join("tree-copy/real.txt").is_file());
        assert!(!h.join("tree-copy/lnk.txt").exists());
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
