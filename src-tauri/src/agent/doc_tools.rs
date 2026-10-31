// Native document generation tools: create_document turns basic Markdown into
// DOCX/PDF/MD/TXT/HTML, create_spreadsheet writes XLSX/CSV from typed rows.
// All writers are pure Rust (docx-rs, rust_xlsxwriter, printpdf) — no
// pandoc/LibreOffice/Python. Project mode keeps every path inside the project
// root (Skip, like write_file); harness mode accepts absolute/~/working-dir-
// relative paths and skips confirmation only inside Harness/ and Projects/
// (CheckConditional, like fs_write). Existing files are only replaced with an
// explicit overwrite=true; content is capped at ~2 MiB.
// (R-12a) The set is split by concern into submodules: markdown (the shared
// block model), docx/pdf/text_html (the document writers), spreadsheet
// (xlsx/csv), paths (the shared path resolution and atomic write helpers) and
// create_document/create_spreadsheet (the tool impls). This facade re-exports
// the tool structs unchanged, so the external import paths
// (agent::doc_tools::…) and the catalog order keep working.
mod create_document;
mod create_spreadsheet;
mod docx;
mod markdown;
mod paths;
mod pdf;
mod spreadsheet;
mod text_html;

pub use create_document::CreateDocumentTool;
pub use create_spreadsheet::CreateSpreadsheetTool;

#[cfg(test)]
mod tests {
    use super::markdown::{parse_markdown, Block};
    use super::pdf::PDF_FONT_CANDIDATES;
    use super::paths::MAX_CONTENT_BYTES;
    use super::{CreateDocumentTool, CreateSpreadsheetTool};
    use crate::agent::tool::{validate_args, PermissionPolicy, Tool, ToolClass, ToolCtx};
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use crate::state::AppState;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn project_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            scope: None,
            cancel: None,
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        (paths::projects_dir(&wd).join("proj"), ctx)
    }

    fn harness_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        (wd.clone(), ToolCtx {
            working_dir: Some(wd),
            ..Default::default()
        })
    }

    fn zip_open(bytes: &[u8]) -> zip::ZipArchive<std::io::Cursor<Vec<u8>>> {
        zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap()
    }

    fn zip_read(ar: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str) -> String {
        let mut f = ar.by_name(name).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    const SAMPLE_MD: &str = "# Report title\n\nIntro paragraph.\n\n## Section one\n\n- bullet alpha\n- bullet beta\n\n1. first step\n2. second step\n\n```\ncode line one\ncode line two\n```\n\n| Name | Qty |\n| --- | --- |\n| widget | 3 |\n";

    #[test]
    fn create_document_docx_is_valid_zip_with_all_block_kinds() {
        let (root, ctx) = project_env("dt-docx");
        let r = CreateDocumentTool::project()
            .execute(
                &json!({
                    "path": "docs/report.docx",
                    "format": "docx",
                    "title": "Doc Title",
                    "markdown": SAMPLE_MD,
                }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["path"], "docs/report.docx");
        assert_eq!(data["format"], "docx");
        assert!(data["bytes"].as_u64().unwrap() > 1_000, "docx too small");

        let file = fs::read(root.join("docs/report.docx")).unwrap();
        let mut ar = zip_open(&file);
        let names: Vec<String> = ar.file_names().map(|s| s.to_string()).collect();
        assert!(names.iter().any(|n| n == "word/document.xml"), "{names:?}");
        let xml = zip_read(&mut ar, "word/document.xml");
        for expected in [
            "Doc Title",
            "Report title",
            "Intro paragraph.",
            "Section one",
            "bullet alpha",
            "first step",
            "code line one",
            "widget",
        ] {
            assert!(xml.contains(expected), "missing {expected:?} in document.xml");
        }
        // title + h1 + para + h2 + 2 bullets + 2 numbered + 2 code + 4 table
        // cells, plus the empty trailer paragraph OOXML requires after a table
        assert_eq!(xml.matches("</w:p>").count(), 15);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn create_document_text_formats_and_extension_mismatch() {
        let (root, ctx) = project_env("dt-text");
        let r = CreateDocumentTool::project()
            .execute(
                &json!({ "path": "a.md", "format": "md", "title": "T", "markdown": "body" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(root.join("a.md")).unwrap(), "# T\n\nbody");

        let r = CreateDocumentTool::project()
            .execute(
                &json!({ "path": "b.txt", "format": "txt", "title": "T", "markdown": "body" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "T\n\nbody");

        let r = CreateDocumentTool::project()
            .execute(
                &json!({ "path": "c.html", "format": "html", "title": "T", "markdown": "a < b & c" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let html = fs::read_to_string(root.join("c.html")).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<pre>a &lt; b &amp; c</pre>"));

        let r = CreateDocumentTool::project()
            .execute(
                &json!({ "path": "d.md", "format": "docx", "markdown": "x" }),
                &ctx,
            )
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("does not match format"), "{}", err.message);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn create_document_pdf_starts_with_pdf_when_font_available() {
        if !PDF_FONT_CANDIDATES.iter().any(|p| Path::new(p).exists()) {
            eprintln!("no candidate PDF font on this machine, skipping");
            return;
        }
        let (root, ctx) = project_env("dt-pdf");
        let long = "Абзац для переноса строк. ".repeat(60);
        let r = CreateDocumentTool::project()
            .execute(
                &json!({ "path": "long.pdf", "format": "pdf", "title": "Отчёт", "markdown": format!("# Заголовок\n\n{long}\n\n- пункт раз\n- пункт два\n") }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(r.data.unwrap()["bytes"].as_u64().unwrap() > 2_000);
        let file = fs::read(root.join("long.pdf")).unwrap();
        assert_eq!(&file[..4], b"%PDF");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn create_spreadsheet_xlsx_is_valid_zip_with_sheets_and_rows() {
        let (root, ctx) = project_env("dt-xlsx");
        let r = CreateSpreadsheetTool::project()
            .execute(
                &json!({
                    "path": "data/sales.xlsx",
                    "sheets": [
                        { "name": "Sales", "rows": [["Region", "Total"], ["north", 12], ["south", null]] },
                        { "name": "Notes", "rows": [["a"]] }
                    ],
                }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["path"], "data/sales.xlsx");
        assert_eq!(data["format"], "xlsx");
        assert!(data["bytes"].as_u64().unwrap() > 1_000, "xlsx too small");

        let file = fs::read(root.join("data/sales.xlsx")).unwrap();
        let mut ar = zip_open(&file);
        let names: Vec<String> = ar.file_names().map(|s| s.to_string()).collect();
        for expected in ["xl/workbook.xml", "xl/worksheets/sheet1.xml", "xl/worksheets/sheet2.xml"] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}: {names:?}");
        }
        // rust_xlsxwriter keeps cell text in sharedStrings.xml, not inline
        let shared = zip_read(&mut ar, "xl/sharedStrings.xml");
        assert!(shared.contains("Region") && shared.contains("north"));
        let sheet1 = zip_read(&mut ar, "xl/worksheets/sheet1.xml");
        assert_eq!(sheet1.matches("<row ").count(), 3);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn create_spreadsheet_csv_quotes_and_rejects_multi_sheet() {
        let (root, ctx) = project_env("dt-csv");
        let r = CreateSpreadsheetTool::project()
            .execute(
                &json!({
                    "path": "out.csv",
                    "sheets": [{ "name": "S", "rows": [["a,b", "he said \"hi\"", "plain"], ["x", 42.5, null]] }],
                }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            fs::read_to_string(root.join("out.csv")).unwrap(),
            "\"a,b\",\"he said \"\"hi\"\"\",plain\nx,42.5,\n"
        );

        let r = CreateSpreadsheetTool::project()
            .execute(
                &json!({
                    "path": "two.csv",
                    "sheets": [{ "name": "A", "rows": [["x"]] }, { "name": "B", "rows": [["y"]] }],
                }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");

        let r = CreateSpreadsheetTool::project()
            .execute(&json!({ "path": "bad.dat", "sheets": [{ "name": "A", "rows": [] }] }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn overwrite_guard_requires_explicit_flag() {
        let (root, ctx) = project_env("dt-overwrite");
        let args = |extra: Value| {
            let mut a = json!({ "path": "note.md", "format": "md", "markdown": "v1" });
            if let Some(obj) = extra.as_object() {
                a.as_object_mut().unwrap().extend(obj.clone());
            }
            a
        };
        let r = CreateDocumentTool::project().execute(&args(json!({})), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let r = CreateDocumentTool::project().execute(&args(json!({})), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("overwrite=true"), "{}", err.message);
        let r = CreateDocumentTool::project()
            .execute(&args(json!({ "overwrite": true, "markdown": "v2" })), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(root.join("note.md")).unwrap(), "v2");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn project_paths_must_stay_inside_the_project() {
        let (root, ctx) = project_env("dt-escape");
        for bad in ["../x.md", "/etc/passwd.md", "a/../../x.md", ".."] {
            let r = CreateDocumentTool::project()
                .execute(&json!({ "path": bad, "format": "md", "markdown": "x" }), &ctx)
                .unwrap();
            assert!(!r.ok, "accepted {bad:?}");
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        assert!(!root.join("../x.md").exists());
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn harness_mode_territory_and_round_trip() {
        let (wd, ctx) = harness_env("dt-harness");
        let tool = CreateDocumentTool::harness();
        let r = tool
            .execute(
                &json!({ "path": "Harness/docs/note.md", "format": "md", "markdown": "h" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(wd.join("Harness/docs/note.md").is_file());

        assert!(tool
            .danger_reason(&json!({ "path": "Harness/docs/note.md" }), &ctx)
            .is_none());
        assert!(tool
            .danger_reason(&json!({ "path": "outside.md" }), &ctx)
            .is_some());

        let sheet = CreateSpreadsheetTool::harness();
        let r = sheet
            .execute(
                &json!({ "path": "Harness/data.csv", "sheets": [{ "name": "S", "rows": [["a", 1]] }] }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(wd.join("Harness/data.csv")).unwrap(), "a,1\n");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn policies_classes_and_schemas_match_the_spec() {
        let project_doc = CreateDocumentTool::project();
        assert_eq!(project_doc.class(), ToolClass::Mutating);
        assert_eq!(project_doc.policy(), PermissionPolicy::Skip);
        assert!(project_doc
            .danger_reason(&json!({ "path": "a.md" }), &ToolCtx::default())
            .is_none());
        let harness_doc = CreateDocumentTool::harness();
        assert_eq!(harness_doc.class(), ToolClass::Mutating);
        assert_eq!(harness_doc.policy(), PermissionPolicy::CheckConditional);
        let harness_sheet = CreateSpreadsheetTool::harness();
        assert_eq!(harness_sheet.class(), ToolClass::Mutating);
        assert_eq!(harness_sheet.policy(), PermissionPolicy::CheckConditional);
        assert_eq!(CreateSpreadsheetTool::project().policy(), PermissionPolicy::Skip);

        let s = project_doc.parameters();
        assert!(validate_args(
            &s,
            &json!({ "path": "a.docx", "format": "docx", "markdown": "x", "overwrite": true })
        )
        .is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.docx", "markdown": "x" }))
            .unwrap_err()
            .contains("format"));
        assert!(validate_args(
            &s,
            &json!({ "path": "a.docx", "format": "exe", "markdown": "x" })
        )
        .unwrap_err()
        .contains("enum"));
        assert!(validate_args(
            &s,
            &json!({ "path": "a.docx", "format": "docx", "markdown": "x", "bogus": 1 })
        )
        .unwrap_err()
        .contains("bogus"));

        let s = CreateSpreadsheetTool::project().parameters();
        assert!(validate_args(
            &s,
            &json!({ "path": "a.xlsx", "sheets": [{ "name": "S", "rows": [["x", 1, null, true]] }] })
        )
        .is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.xlsx" }))
            .unwrap_err()
            .contains("sheets"));
        assert!(validate_args(
            &s,
            &json!({ "path": "a.xlsx", "sheets": [{ "name": "S", "rows": [], "extra": 1 }] })
        )
        .unwrap_err()
        .contains("extra"));
    }

    #[test]
    fn content_cap_rejects_oversized_input() {
        let (_root, ctx) = project_env("dt-cap");
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let r = CreateDocumentTool::project()
            .execute(&json!({ "path": "big.md", "format": "md", "markdown": big }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("cap"), "{}", err.message);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn markdown_parser_blocks() {
        let blocks = parse_markdown(SAMPLE_MD);
        assert_eq!(
            blocks,
            vec![
                Block::Heading(1, "Report title".into()),
                Block::Paragraph("Intro paragraph.".into()),
                Block::Heading(2, "Section one".into()),
                Block::Bullets(vec!["bullet alpha".into(), "bullet beta".into()]),
                Block::Numbered(vec!["first step".into(), "second step".into()]),
                Block::Code(vec!["code line one".into(), "code line two".into()]),
                Block::Table(vec![
                    vec!["Name".into(), "Qty".into()],
                    vec!["widget".into(), "3".into()],
                ]),
            ]
        );
    }
}
