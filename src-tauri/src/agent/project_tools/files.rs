// Project file/folder tools: search, read_file, list_tree, write_file,
// create_folder, rename_path, delete_path. Every path argument is resolved
// strictly inside the project root by the fs_ops resolvers; absolute OS paths
// are never exposed to the model.
use super::support::{
    emit_trash_changed, emit_versions_changed, envelope, fail, require_project, require_wd,
    state_of, with_conn,
};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::agent::task_glue::is_missing;
use crate::error::Result;
use crate::{extract, fs_ops, search, undo};
use serde_json::{json, Value};
use std::path::Path;

// Envelope headroom for extracted documents: the JSON wrapper around the
// content (keys, path, escaping) costs a little, so the extracted text is
// capped below max_result_len to keep the capped envelope inside the limit.
const EXTRACT_ENVELOPE_RESERVE: usize = 512;

pub struct SearchTool;

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search this project's index (FTS + semantic) and return hits with relPath, name, snippet and matchTypes."
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
        let pid = require_project(ctx)?;
        let state = state_of(ctx)?;
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        match search::search_project(state, pid, query, limit) {
            Ok(hits) => Ok(ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len())),
            Err(e) => Ok(fail(e)),
        }
    }
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file inside the project by project-relative path. Text files are returned as-is; docx/xlsx/xlsm/pdf are extracted to text (extracted: true). Images and other binary files are rejected."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": ["rel_path"],
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
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        // Resolve strictly inside the project first (not_found / escape /
        // folder errors map through fail()); the canonical path is what the
        // document extractor reads.
        let path = match with_conn(ctx, |conn| fs_ops::resolve_existing(conn, &wd, pid, rel)) {
            Ok(path) => path,
            Err(e) => return Ok(fail(e)),
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if fs_ops::IMAGE_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                crate::agent::harness_tools::IMAGE_READ_HINT,
            ));
        }
        match extract::kind(&path) {
            Some("docx") | Some("xlsx") | Some("pdf") => {
                let max_chars = self.max_result_len() - EXTRACT_ENVELOPE_RESERVE;
                match extract::extract_text(&path, max_chars) {
                    Ok(content) => Ok(ToolResult::ok(json!({
                        "relPath": rel,
                        "content": content,
                        "extracted": true,
                        "format": ext,
                    }))
                    .capped(self.max_result_len())),
                    Err(e) => Ok(fail(e)),
                }
            }
            // Plain text (md/txt/csv/... and unknown non-image extensions):
            // read_text validates UTF-8 and rejects NUL bytes as before.
            _ => {
                let r = with_conn(ctx, |conn| fs_ops::read_text(conn, &wd, pid, rel));
                Ok(match r {
                    Ok(content) => {
                        ToolResult::ok(json!({ "relPath": rel, "content": content }))
                            .capped(self.max_result_len())
                    }
                    Err(e) => fail(e),
                })
            }
        }
    }
}

pub struct ListTreeTool;

impl Tool for ListTreeTool {
    fn name(&self) -> &str {
        "list_tree"
    }
    fn description(&self) -> &str {
        "List a project folder (empty rel_path = project root): name, kind, ext, size per entry."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": [],
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
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args.get("rel_path").and_then(Value::as_str).unwrap_or("");
        let r = with_conn(ctx, |conn| fs_ops::list_dir(conn, &wd, pid, rel));
        Ok(match r {
            Ok(entries) => {
                ToolResult::ok(json!({ "relPath": rel, "entries": entries })).capped(self.max_result_len())
            }
            Err(e) => fail(e),
        })
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file inside the project. The parent folder must already exist."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rel_path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["rel_path", "content"],
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
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        let path = Path::new(rel);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Snapshot the previous content before an overwrite (version history);
        // a missing file stays unversioned, a binary read just skips the save.
        let previous: Option<(String, String)> =
            with_conn(ctx, |conn| fs_ops::resolve_existing(conn, &wd, pid, rel))
                .ok()
                .and_then(|p| {
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|c| (p.display().to_string(), c))
                });
        let attempt = with_conn(ctx, |conn| fs_ops::write_text(conn, &wd, pid, rel, content));
        let result = match attempt {
            // fs_ops::write_text only overwrites; create the file first when missing.
            Err(e) if is_missing(&e) => with_conn(ctx, |conn| {
                fs_ops::create_file(conn, &wd, pid, &parent, &name)?;
                fs_ops::write_text(conn, &wd, pid, rel, content)
            }),
            other => other,
        };
        if let Ok(entry) = &result {
            if let Some((abs, prev)) = &previous {
                let version_id = with_conn(ctx, |conn| {
                    crate::reliability::version_save(conn, &wd, "project", abs, prev)
                })
                .ok();
                if let Some(version_id) = version_id {
                    emit_versions_changed(ctx, "project", abs);
                    let _ = with_conn(ctx, |conn| {
                        undo::push_file_write(
                            conn,
                            "project",
                            abs,
                            Some(version_id),
                            false,
                            Some(pid),
                            Some(&entry.rel_path),
                        )
                    });
                }
            } else {
                let abs = with_conn(ctx, |conn| {
                    fs_ops::resolve_existing(conn, &wd, pid, &entry.rel_path)
                        .map(|p| p.display().to_string())
                })
                .ok();
                if let Some(abs) = abs {
                    let _ = with_conn(ctx, |conn| {
                        undo::push_file_write(
                            conn,
                            "project",
                            &abs,
                            None,
                            true,
                            Some(pid),
                            Some(&entry.rel_path),
                        )
                    });
                }
            }
        }
        Ok(match result {
            Ok(entry) => ToolResult::ok(json!({ "entry": entry })),
            Err(e) => fail(e),
        })
    }
}

pub struct CreateFolderTool;

impl Tool for CreateFolderTool {
    fn name(&self) -> &str {
        "create_folder"
    }
    fn description(&self) -> &str {
        "Create a folder inside the project under parent_rel (empty parent_rel = project root)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent_rel": { "type": "string" },
                "name": { "type": "string" }
            },
            "required": ["parent_rel", "name"],
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
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let parent = args["parent_rel"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let r = with_conn(ctx, |conn| fs_ops::create_folder(conn, &wd, pid, parent, name));
        Ok(envelope(r))
    }
}

pub struct RenamePathTool;

impl Tool for RenamePathTool {
    fn name(&self) -> &str {
        "rename_path"
    }
    fn description(&self) -> &str {
        "Rename a file or folder inside the project to new_name (stays in the same parent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rel_path": { "type": "string" },
                "new_name": { "type": "string" }
            },
            "required": ["rel_path", "new_name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let new_name = args["new_name"].as_str().unwrap_or_default();
        let from_abs = with_conn(ctx, |conn| {
            fs_ops::resolve_existing(conn, &wd, pid, rel).map(|p| p.display().to_string())
        })
        .ok();
        let r = with_conn(ctx, |conn| fs_ops::rename(conn, &wd, pid, rel, new_name));
        if let (Ok(entry), Some(from_abs)) = (&r, from_abs) {
            let to_abs = with_conn(ctx, |conn| {
                fs_ops::resolve_existing(conn, &wd, pid, &entry.rel_path)
                    .map(|p| p.display().to_string())
            })
            .ok();
            if let Some(to_abs) = to_abs {
                let _ = with_conn(ctx, |conn| {
                    undo::push_rename(
                        conn,
                        "project",
                        &from_abs,
                        &to_abs,
                        Some(pid),
                        Some(&entry.rel_path),
                    )
                });
            }
        }
        Ok(envelope(r))
    }
}

pub struct DeletePathTool;

impl Tool for DeletePathTool {
    fn name(&self) -> &str {
        "delete_path"
    }
    fn description(&self) -> &str {
        "Delete a file or folder (recursively) inside the project. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": ["rel_path"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let path_abs = with_conn(ctx, |conn| {
            fs_ops::resolve_existing(conn, &wd, pid, rel).map(|p| p.display().to_string())
        })
        .ok();
        let r = with_conn(ctx, |conn| fs_ops::delete(conn, &wd, pid, rel));
        if let Ok(id) = &r {
            if let Some(path_abs) = path_abs {
                let _ = with_conn(ctx, |conn| {
                    undo::push_delete(conn, *id, "project", &path_abs, Some(pid), Some(rel))
                });
            }
            emit_trash_changed(ctx, Some(*id));
        }
        Ok(match r {
            Ok(id) => ToolResult::ok(json!({ "deleted": rel, "trashId": id })),
            Err(e) => fail(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_harness::{exec, root, setup};
    use super::*;
    use crate::index;
    use serde_json::json;
    use std::fs;
    use std::io::Write as _;

    #[test]
    fn search_read_and_list_round_trip() {
        let env = setup("pt-search");
        fs::create_dir_all(root(&env.wd).join("notes")).unwrap();
        fs::write(
            root(&env.wd).join("notes/plan.md"),
            "access data for the staging server example.ru",
        )
        .unwrap();
        index::index_project(env.ctx.state.as_deref().unwrap(), env.ctx.project_id.unwrap())
            .unwrap();

        let r = exec("search", json!({ "query": "staging" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits[0]["relPath"], "notes/plan.md");
        assert!(hits[0]["matchTypes"].as_array().unwrap().contains(&json!("fts")));
        assert!(hits[0]["snippet"].as_str().unwrap().contains("staging"));

        let r = exec("read_file", json!({ "rel_path": "notes/plan.md" }), &env.ctx);
        assert!(r.ok);
        assert!(r.data.unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("example.ru"));

        let r = exec("list_tree", json!({}), &env.ctx);
        assert!(r.ok);
        let entries = r.data.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"notes"));
        assert!(names.contains(&"datas.md"));

        // binary files are refused with execution_error
        fs::write(root(&env.wd).join("bin.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "bin.dat" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "execution_error");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            zip.start_file(name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn docx_bytes(text: &str) -> Vec<u8> {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
        );
        zip_bytes(&[("word/document.xml", &xml)])
    }

    fn xlsx_bytes(text: &str) -> Vec<u8> {
        let shared = format!(
            r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>{text}</t></si></sst>"#
        );
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#;
        zip_bytes(&[("xl/sharedStrings.xml", &shared), ("xl/worksheets/sheet1.xml", sheet)])
    }

    #[test]
    fn read_file_resolves_special_document_name_variants_and_reports_ambiguity() {
        let env = setup("pt-lenient-docs");
        let dir = root(&env.wd).join("02_Договоры");
        fs::create_dir_all(&dir).unwrap();
        // on-disk name carries NBSP + em dash; the agent types hyphen + spaces
        let real = "Договор № 017-2026\u{a0}— копия.docx";
        fs::write(dir.join(real), docx_bytes("Договор text")).unwrap();
        let r = exec(
            "read_file",
            json!({ "rel_path": "02_Договоры/Договор № 017-2026 - копия.docx" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Договор text");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "docx");

        // a second file differing only in the dash makes the variant ambiguous:
        // refused with the candidate list, never guessed
        fs::write(
            dir.join("Договор № 017-2026\u{a0}\u{2013} копия.docx"),
            docx_bytes("en"),
        )
        .unwrap();
        let r = exec(
            "read_file",
            json!({ "rel_path": "02_Договоры/Договор № 017-2026 - копия.docx" }),
            &env.ctx,
        );
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("ambiguous"), "{}", err.message);
        assert!(err.message.contains(real), "{}", err.message);
    }

    #[test]
    fn read_file_extracts_documents_caps_and_rejects_images_and_binaries() {
        let env = setup("pt-docs");
        fs::create_dir_all(root(&env.wd).join("docs")).unwrap();

        // plain text keeps the old shape: no extracted/format keys
        fs::write(root(&env.wd).join("docs/plan.md"), "plain text body").unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/plan.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "plain text body");
        assert_eq!(data["relPath"], "docs/plan.md");
        assert!(data.get("extracted").is_none());
        assert!(data.get("format").is_none());

        // docx built in memory: extracted text plus the extracted/format keys
        fs::write(root(&env.wd).join("docs/report.docx"), docx_bytes("Hello docx world")).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/report.docx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Hello docx world");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "docx");
        assert_eq!(data["relPath"], "docs/report.docx");

        // xlsx likewise
        fs::write(root(&env.wd).join("docs/sheet.xlsx"), xlsx_bytes("Report Q")).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/sheet.xlsx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Report Q");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "xlsx");

        // images get the vision hint instead of base64
        fs::write(root(&env.wd).join("docs/pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/pic.png" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("vision-capable"), "{}", err.message);

        // unknown binary extensions keep the old clear rejection
        fs::write(root(&env.wd).join("docs/blob.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/blob.dat" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("binary"), "{}", err.message);

        // unsupported document extension (legacy .doc) is rejected too
        fs::write(root(&env.wd).join("docs/legacy.doc"), b"old binary format\x00").unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/legacy.doc" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "execution_error");

        // max_chars cap: a long docx paragraph is cut to max_result_len
        fs::write(root(&env.wd).join("docs/long.docx"), docx_bytes(&"x".repeat(9_000))).unwrap();
        let r = ReadFileTool
            .execute(&json!({ "rel_path": "docs/long.docx" }), &env.ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(
            data["content"].as_str().unwrap().chars().count(),
            8_000 - EXTRACT_ENVELOPE_RESERVE
        );
        assert_eq!(data["extracted"], true);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn write_create_folder_rename_delete_round_trip() {
        let env = setup("pt-fs");
        let r = exec("create_folder", json!({ "parent_rel": "", "name": "docs" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["kind"], "dir");

        // create via write_file, then overwrite
        let r = exec(
            "write_file",
            json!({ "rel_path": "docs/note.md", "content": "v1" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["entry"]["relPath"], "docs/note.md");
        let r = exec(
            "write_file",
            json!({ "rel_path": "docs/note.md", "content": "v2" }),
            &env.ctx,
        );
        assert!(r.ok);
        assert_eq!(fs::read_to_string(root(&env.wd).join("docs/note.md")).unwrap(), "v2");

        let r = exec(
            "rename_path",
            json!({ "rel_path": "docs/note.md", "new_name": "renamed.md" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["relPath"], "docs/renamed.md");

        let r = exec("delete_path", json!({ "rel_path": "docs/renamed.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!root(&env.wd).join("docs/renamed.md").exists());

        // The overwrite snapshotted the previous content into the version
        // store (keyed by the canonical path), the delete landed in the
        // trash, and both panels got their change events.
        let state = env.ctx.state.clone().unwrap();
        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let abs = root(&env.wd)
            .join("docs")
            .canonicalize()
            .unwrap()
            .join("note.md")
            .display()
            .to_string();
        assert_eq!(crate::reliability::version_list(conn, "project", &abs).unwrap().len(), 1);
        assert_eq!(crate::reliability::trash_list(conn).unwrap().len(), 1);
        drop(guard);
        let events: Vec<String> =
            env.events.lock().unwrap().iter().map(|(e, _)| e.clone()).collect();
        assert!(events.iter().any(|e| e == "versions-changed"), "{events:?}");
        assert!(events.iter().any(|e| e == "trash-changed"), "{events:?}");

        // missing file reads back as not_found
        let r = exec("read_file", json!({ "rel_path": "docs/renamed.md" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn path_escapes_are_rejected_with_permission_denied() {
        let env = setup("pt-escape");
        for bad in ["../x", "/etc/passwd", "a/../../x", ".."] {
            for name in ["read_file", "write_file", "delete_path", "list_tree"] {
                let args = if name == "write_file" {
                    json!({ "rel_path": bad, "content": "x" })
                } else {
                    json!({ "rel_path": bad })
                };
                let r = exec(name, args, &env.ctx);
                assert!(!r.ok, "{name} accepted {bad:?}");
                assert_eq!(
                    r.error.unwrap().code,
                    "permission_denied",
                    "{name} on {bad:?}"
                );
            }
        }
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
