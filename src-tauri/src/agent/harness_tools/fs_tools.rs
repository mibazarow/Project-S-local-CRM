// Machine-wide fs tools (SPEC 5.2 items 1, 2): fs_read/fs_list/fs_grep are
// read-only and skip confirmation; fs_write/fs_delete/fs_copy/fs_move/
// fs_rename/fs_mkdir mutate. Paths are absolute, ~-expanded or working-dir
// relative; the confirmation policy keys off in_default_territory (mod.rs).
// The shared fs_copy/fs_move body lives in transfer.rs.
// (R-12b) The set is split by concern into submodules: read/list/grep (the
// read-only trio), write/mkdir (create), delete, rename and transfer_tools
// (the fs_copy/fs_move wrappers over transfer.rs). This facade re-exports the
// tool structs unchanged, so the external import paths
// (harness_tools::fs_tools::…) and the harness_catalog order keep working.
mod delete;
mod grep;
mod list;
mod mkdir;
mod read;
mod rename;
mod transfer_tools;
mod write;

pub use self::delete::FsDeleteTool;
pub use self::grep::FsGrepTool;
pub use self::list::FsListTool;
pub use self::mkdir::FsMkdirTool;
pub use self::read::FsReadTool;
pub use self::rename::FsRenameTool;
pub use self::transfer_tools::{FsCopyTool, FsMoveTool};
pub use self::write::FsWriteTool;

// Resolves the parent folders of a create/write target leniently, keeping
// the final component as requested; a genuinely missing parent chain is
// returned unchanged so the caller creates it. Shared by write and mkdir.
fn lenient_target_parent(path: &Path) -> Result<PathBuf> {
    crate::path_policy::lenient_parent(path)
}

// Re-exported so the submodules keep the same `use super::{…}` shape the flat
// file had; the primitives themselves live in harness_tools/mod.rs.
pub(crate) use super::{
    canonicalish, envelope_r, in_default_territory, lenient_existing, lenient_fail, normalize,
    resolve_path, IMAGE_READ_HINT,
};

use crate::error::Result;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::read::EXTRACT_ENVELOPE_RESERVE;
    use crate::agent::harness_tools::test_support::{exec, setup, tool};
    use crate::agent::tool::validate_args;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Write as _;

    #[test]
    fn fs_write_read_list_delete_round_trip_inside_and_outside() {
        let env = setup("ht-fs");
        let inside = env.wd.join("Harness").join("sub").join("note.md");
        let r = exec("fs_write", json!({ "path": "Harness/sub/note.md", "content": "v1" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(&inside).unwrap(), "v1");

        let r = exec("fs_read", json!({ "path": "Harness/sub/note.md" }), &env.ctx);
        assert_eq!(r.data.unwrap()["content"], "v1");

        // outside the territory: execute itself still works (the loop, not
        // the tool, enforces the confirmation)
        let outside = env.wd.join("outside.txt");
        let r = exec("fs_write", json!({ "path": "outside.txt", "content": "v2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "v2");

        let r = exec("fs_list", json!({ "path": "Harness/sub" }), &env.ctx);
        let entries = r.data.unwrap()["entries"].as_array().unwrap().clone();
        assert_eq!(entries[0]["name"], "note.md");
        assert_eq!(entries[0]["kind"], "file");

        let r = exec("fs_delete", json!({ "path": "outside.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!outside.exists());
        let r = exec("fs_delete", json!({ "path": "Harness/sub" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!inside.exists());

        // obviously destructive targets are refused even with confirmation
        for bad in ["", ".", "~", "/"] {
            let r = exec("fs_delete", json!({ "path": bad }), &env.ctx);
            assert!(!r.ok, "accepted {bad:?}");
        }
        let r = exec("fs_read", json!({ "path": "missing.md" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_write_inside_project_emits_fs_changed_outside_does_not() {
        let env = setup("ht-fs-write-fs-changed");
        let state = env.ctx.state.clone().unwrap();
        let pid = {
            let guard = crate::sync_util::lock_recover(&state.db);
            crate::projects::create_project(guard.as_ref().unwrap(), &env.wd, "proj")
                .unwrap()
                .id
        };
        let r = exec(
            "fs_write",
            json!({ "path": "Projects/proj/note.md", "content": "v1" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec(
            "fs_write",
            json!({ "path": "Harness/out.md", "content": "v1" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let changed: Vec<Value> = env
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "fs-changed")
            .map(|(_, payload)| payload.clone())
            .collect();
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0]["projectId"], pid);
        assert_eq!(changed[0]["relPath"], "note.md");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_rename_files_dirs_collisions_and_invalid_names() {
        let env = setup("ht-rename");
        let h = env.wd.join("Harness");

        // rename a file inside the same parent
        fs::write(h.join("a.txt"), "hello").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/a.txt", "newName": "b.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["from"], h.join("a.txt").display().to_string());
        assert_eq!(data["to"], h.join("b.txt").display().to_string());
        assert!(!h.join("a.txt").exists());
        assert_eq!(fs::read_to_string(h.join("b.txt")).unwrap(), "hello");

        // rename a directory: the whole tree moves with it
        fs::create_dir_all(h.join("dir/sub")).unwrap();
        fs::write(h.join("dir/sub/f.txt"), "x").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/dir", "newName": "dir2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!h.join("dir").exists());
        assert_eq!(fs::read_to_string(h.join("dir2/sub/f.txt")).unwrap(), "x");

        // renaming to the same name is a no-op success
        let r = exec("fs_rename", json!({ "path": "Harness/b.txt", "newName": "b.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);

        // collision with an existing entry is refused, both sides survive
        fs::write(h.join("c.txt"), "c").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/b.txt", "newName": "c.txt" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("destination exists"), "{}", err.message);
        assert!(h.join("b.txt").is_file() && h.join("c.txt").is_file());

        // invalid names: separators, traversal, empty, control characters
        for bad in ["x/y", "..", ".", "", "a\\b", "x\u{0}y"] {
            let r = exec("fs_rename", json!({ "path": "Harness/c.txt", "newName": bad }), &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "newName {bad:?}");
        }
        assert!(h.join("c.txt").is_file());

        // missing source maps to not_found
        let r = exec("fs_rename", json!({ "path": "Harness/missing.txt", "newName": "x" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // schema: both fields required, extras refused
        let schema = tool("fs_rename").parameters();
        assert!(validate_args(&schema, &json!({ "path": "Harness/a.txt", "newName": "b" })).is_ok());
        assert!(validate_args(&schema, &json!({ "path": "Harness/a.txt" }))
            .unwrap_err()
            .contains("newName"));
        assert!(validate_args(&schema, &json!({ "path": "a", "newName": "b", "overwrite": true }))
            .unwrap_err()
            .contains("overwrite"));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_mkdir_requires_existing_parent_and_refuses_existing() {
        let env = setup("ht-mkdir");
        let h = env.wd.join("Harness");

        let r = exec("fs_mkdir", json!({ "path": "Harness/newdir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["created"], true);
        assert!(h.join("newdir").is_dir());

        // an existing folder or file is refused
        let r = exec("fs_mkdir", json!({ "path": "Harness/newdir" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("already exists"), "{}", err.message);
        fs::write(h.join("afile.txt"), "x").unwrap();
        let r = exec("fs_mkdir", json!({ "path": "Harness/afile.txt" }), &env.ctx);
        assert!(
            r.error.unwrap().message.contains("already exists as a file"),
            "must name the kind"
        );

        // parents are not created
        let r = exec("fs_mkdir", json!({ "path": "Harness/no/such/parent" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("parent folder does not exist"), "{}", err.message);
        assert!(!h.join("no").exists());

        // empty path is refused
        let r = exec("fs_mkdir", json!({ "path": "   " }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // inside Projects behaves like inside Harness
        let r = exec("fs_mkdir", json!({ "path": "Projects/pdir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);

        // schema: path required, extras refused
        let schema = tool("fs_mkdir").parameters();
        assert!(validate_args(&schema, &json!({ "path": "Harness/x" })).is_ok());
        assert!(validate_args(&schema, &json!({})).unwrap_err().contains("path"));
        assert!(validate_args(&schema, &json!({ "path": "x", "parents": true }))
            .unwrap_err()
            .contains("parents"));
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
    fn fs_read_resolves_lenient_name_variants_and_refuses_ambiguity() {
        let env = setup("ht-lenient");
        let real = "Harness/Договор № 017-2026\u{a0}— копия.txt";
        fs::write(env.wd.join(real), "lenient body").unwrap();
        let r = exec(
            "fs_read",
            json!({ "path": "Harness/Договор № 017-2026 - копия.txt" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["content"], "lenient body");

        // a second file differing only in the dash makes the variant ambiguous
        fs::write(
            env.wd.join("Harness/Договор № 017-2026\u{a0}\u{2013} копия.txt"),
            "en",
        )
        .unwrap();
        let r = exec(
            "fs_read",
            json!({ "path": "Harness/Договор № 017-2026 - копия.txt" }),
            &env.ctx,
        );
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("ambiguous"), "{}", err.message);
    }

    #[test]
    fn fs_read_extracts_documents_caps_and_rejects_images_and_binaries() {
        let env = setup("ht-docs");
        let dir = env.wd.join("Harness").join("docs");
        fs::create_dir_all(&dir).unwrap();

        // plain text keeps the old shape: no extracted/format keys
        fs::write(dir.join("note.md"), "plain harness text").unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/note.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "plain harness text");
        assert!(data.get("extracted").is_none());
        assert!(data.get("format").is_none());

        // docx built in memory: extracted text plus the extracted/format keys
        fs::write(dir.join("report.docx"), docx_bytes("Harness docx body")).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/report.docx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Harness docx body");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "docx");

        // xlsx likewise
        fs::write(dir.join("sheet.xlsx"), xlsx_bytes("Q3 plan")).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/sheet.xlsx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Q3 plan");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "xlsx");

        // images get the vision hint instead of base64
        fs::write(dir.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/pic.png" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("vision-capable"), "{}", err.message);

        // unknown binary extensions keep the old clear rejection
        fs::write(dir.join("blob.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/blob.dat" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("binary"), "{}", err.message);

        // max_chars cap: a long docx paragraph is cut to max_result_len
        fs::write(dir.join("long.docx"), docx_bytes(&"y".repeat(9_000))).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/long.docx" }), &env.ctx);
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
    fn fs_grep_finds_pattern_and_respects_max_results() {
        let env = setup("ht-grep");
        let dir = env.wd.join("Harness").join("tree");
        fs::create_dir_all(dir.join("deep")).unwrap();
        fs::write(dir.join("a.txt"), "one NEEDLE one").unwrap();
        fs::write(dir.join("deep/b.txt"), "two needle two\nthree NEEDLE three").unwrap();
        fs::write(dir.join("c.txt"), "no match").unwrap();

        let r = exec("fs_grep", json!({ "pattern": "needle" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let matches = data["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3, "{matches:?}");
        assert_eq!(data["truncated"], false);

        let r = exec("fs_grep", json!({ "pattern": "needle", "max_results": 2 }), &env.ctx);
        let data = r.data.unwrap();
        assert_eq!(data["matches"].as_array().unwrap().len(), 2);
        assert_eq!(data["truncated"], true);
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
