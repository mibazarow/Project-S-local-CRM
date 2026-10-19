// pdf_to_image (Q.md computer-control slice): renders PDF pages to PNG.
// pdftoppm (poppler) renders any page list ("1-3", "1,3"); without it sips
// and then qlmanage render only the first page and the result carries a
// note saying so. The source is validated twice (.pdf extension and the
// %PDF magic bytes), dpi is clamped to 1..300 (default 150), the page list
// to 50 pages, and every CLI call runs as argv under a timeout. Output
// defaults to Settings/tmp/pdf-<stamp>/, so the tool is Skip: the source is
// read-only and the writes land in the app's own tmp folder.
use super::archive_tools::find_binary;
use super::system_tools::run_cmd;
use crate::agent::project_tools::require_wd;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::path_policy::resolve_path;
use crate::paths;
use chrono::Utc;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SIPS_BINARY: &str = "/usr/bin/sips";
const QLMANAGE_BINARY: &str = "/usr/bin/qlmanage";
const RENDER_TIMEOUT: Duration = Duration::from_secs(60);
const DPI_DEFAULT: u64 = 150;
const DPI_MAX: u64 = 300;
const MAX_PAGES: usize = 50;

fn find_pdftoppm() -> Option<PathBuf> {
    find_binary("pdftoppm", &["/opt/homebrew/bin/pdftoppm", "/usr/local/bin/pdftoppm"])
}

// "1-3", "1,3", "2" -> sorted unique pages; empty defaults to page 1.
fn parse_pages(spec: &str) -> std::result::Result<Vec<u32>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(vec![1]);
    }
    let mut pages: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty page range in the pages list".into());
        }
        match part.split_once('-') {
            Some((from, to)) => {
                let from = from
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| format!("invalid page range \"{part}\""))?;
                let to = to
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| format!("invalid page range \"{part}\""))?;
                if from < 1 || to < from {
                    return Err(format!("page range \"{part}\" must be 1-based and non-empty"));
                }
                pages.extend(from..=to);
            }
            None => {
                let page = part
                    .parse::<u32>()
                    .map_err(|_| format!("invalid page number \"{part}\""))?;
                if page < 1 {
                    return Err(format!("page numbers are 1-based, got {page}"));
                }
                pages.push(page);
            }
        }
    }
    pages.sort();
    pages.dedup();
    if pages.len() > MAX_PAGES {
        return Err(format!("too many pages ({}); the limit is {MAX_PAGES}", pages.len()));
    }
    Ok(pages)
}

fn dpi_value(raw: Option<u64>) -> u32 {
    raw.unwrap_or(DPI_DEFAULT).clamp(1, DPI_MAX) as u32
}

// %PDF magic within the first 1024 bytes (the spec allows leading junk).
fn has_pdf_magic(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(4).any(|w| w == b"%PDF")
}

// PNG IHDR dimensions straight from the file header.
fn png_dims(path: &Path) -> Option<(u32, u32)> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

#[derive(Clone)]
pub struct PdfToImageTool {
    // None = auto-detect pdftoppm on PATH/brew prefixes; tests inject paths.
    pdftoppm: Option<PathBuf>,
    sips: PathBuf,
    qlmanage: PathBuf,
}

impl Default for PdfToImageTool {
    fn default() -> Self {
        Self {
            pdftoppm: None,
            sips: PathBuf::from(SIPS_BINARY),
            qlmanage: PathBuf::from(QLMANAGE_BINARY),
        }
    }
}

impl PdfToImageTool {
    #[cfg(test)]
    fn with_renderers(pdftoppm: Option<PathBuf>, sips: PathBuf, qlmanage: PathBuf) -> Self {
        Self {
            pdftoppm,
            sips,
            qlmanage,
        }
    }
}

impl Tool for PdfToImageTool {
    fn name(&self) -> &str {
        "pdf_to_image"
    }
    fn description(&self) -> &str {
        "Render PDF pages to PNG via pdftoppm (poppler), falling back to sips/qlmanage for the first page.\npages is a list like \"1-3\" or \"1,3\" (default \"1\", max 50 pages), dpi default 150 (max 300), output_dir defaults to Settings/tmp/pdf-<stamp>/; poppler is installed with `brew install poppler`. The source must be a real .pdf. Read-only source, writes to the app tmp; returns {ok, pages:[{page,path,width,height}], renderer, note?}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pdf": { "type": "string" },
                "pages": { "type": "string" },
                "dpi": { "type": "integer" },
                "output_dir": { "type": "string" }
            },
            "required": ["pdf"],
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
        let wd = require_wd(ctx)?;
        let raw = args.get("pdf").and_then(Value::as_str).unwrap_or("");
        if raw.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "pdf path is required"));
        }
        let pdf = resolve_path(raw, &wd);
        match fs::symlink_metadata(&pdf) {
            Ok(m) if m.is_dir() => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} is a directory", pdf.display()),
                ))
            }
            Ok(_) => {}
            Err(_) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::NotFound,
                    format!("{} not found", pdf.display()),
                ))
            }
        }
        if pdf.extension().map(|e| e.to_string_lossy().to_lowercase()) != Some("pdf".to_string()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("\"{}\" must end with .pdf", pdf.display()),
            ));
        }
        if !has_pdf_magic(&pdf) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} is not a PDF file (no %PDF magic)", pdf.display()),
            ));
        }
        let pages = match parse_pages(args.get("pages").and_then(Value::as_str).unwrap_or("")) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let dpi = dpi_value(args.get("dpi").and_then(Value::as_u64));
        let out_dir = match args.get("output_dir").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => resolve_path(s, &wd),
            _ => paths::settings_tmp_dir(&wd).join(format!("pdf-{}", Utc::now().format("%Y%m%d-%H%M%S"))),
        };
        if let Err(e) = fs::create_dir_all(&out_dir) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("cannot create {}: {e}", out_dir.display()),
            ));
        }
        let stem = pdf
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "page".to_string());

        let pdftoppm = self
            .pdftoppm
            .clone()
            .or_else(find_pdftoppm)
            .filter(|p| p.is_file());
        let mut rendered: Vec<Value> = Vec::new();
        let mut renderer = "pdftoppm";
        let mut note: Option<String> = None;
        if let Some(bin) = pdftoppm {
            for page in &pages {
                let prefix = out_dir.join(&stem);
                let argv = vec![
                    "-png".to_string(),
                    "-r".to_string(),
                    dpi.to_string(),
                    "-f".to_string(),
                    page.to_string(),
                    "-l".to_string(),
                    page.to_string(),
                    pdf.display().to_string(),
                    prefix.display().to_string(),
                ];
                let out = match run_cmd(&bin, &argv, None, RENDER_TIMEOUT) {
                    Ok(o) => o,
                    Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
                };
                let file = out_dir.join(format!("{stem}-{page}.png"));
                if out.code != Some(0) || !file.is_file() {
                    return Ok(ToolResult::err(
                        ToolErrorCode::ExecutionError,
                        format!(
                            "pdftoppm failed for page {page} (exit {}): {}",
                            out.code.unwrap_or(-1),
                            out.stderr.trim()
                        ),
                    ));
                }
                rendered.push(page_entry(*page, &file));
            }
        } else if pages.iter().any(|p| *p != 1) {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                "pdftoppm (poppler) not found; install it with `brew install poppler` to render pages beyond the first",
            ));
        } else if self.sips.is_file() {
            renderer = "sips";
            note = Some("pdftoppm (poppler) not found; rendered only page 1 via sips".to_string());
            let file = out_dir.join(format!("{stem}-1.png"));
            let argv = vec![
                "-s".to_string(),
                "format".to_string(),
                "png".to_string(),
                pdf.display().to_string(),
                "--out".to_string(),
                file.display().to_string(),
            ];
            let out = match run_cmd(&self.sips, &argv, None, RENDER_TIMEOUT) {
                Ok(o) => o,
                Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
            };
            if out.code != Some(0) || !file.is_file() {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!(
                        "sips failed (exit {}): {}",
                        out.code.unwrap_or(-1),
                        out.stderr.trim()
                    ),
                ));
            }
            rendered.push(page_entry(1, &file));
        } else if self.qlmanage.is_file() {
            renderer = "qlmanage";
            note = Some("pdftoppm and sips not found; rendered a first-page thumbnail via qlmanage".to_string());
            let argv = vec![
                "-t".to_string(),
                "-s".to_string(),
                (dpi as u64 * 2).to_string(),
                "-o".to_string(),
                out_dir.display().to_string(),
                pdf.display().to_string(),
            ];
            let out = match run_cmd(&self.qlmanage, &argv, None, RENDER_TIMEOUT) {
                Ok(o) => o,
                Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
            };
            let file = out_dir.join(format!(
                "{}.png",
                pdf.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
            ));
            if out.code != Some(0) || !file.is_file() {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!(
                        "qlmanage failed (exit {}): {}",
                        out.code.unwrap_or(-1),
                        out.stderr.trim()
                    ),
                ));
            }
            rendered.push(page_entry(1, &file));
        } else {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                "no PDF renderer found: pdftoppm (`brew install poppler`), sips or qlmanage",
            ));
        }
        let mut data = json!({
            "ok": true,
            "pages": rendered,
            "renderer": renderer,
            "output_dir": out_dir.display().to_string(),
        });
        if let Some(n) = note {
            data["note"] = json!(n);
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

fn page_entry(page: u32, file: &Path) -> Value {
    let mut entry = json!({ "page": page, "path": file.display().to_string() });
    if let Some((w, h)) = png_dims(file) {
        entry["width"] = json!(w);
        entry["height"] = json!(h);
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;

    // Minimal one-page PDF with a valid xref (same shape as the pdf_tools
    // fixtures), written by the live tests.
    fn write_pdf_fixture(path: &Path) {
        let content = b"BT /F1 24 Tf 20 100 Td (Hello PDF) Tj ET\n";
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
            format!("<< /Length {} >>\nstream\n", content.len()).into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
        let mut offs = Vec::new();
        for (i, obj) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(obj);
            if i == 3 {
                out.extend_from_slice(content);
                out.extend_from_slice(b"endstream\n");
            } else {
                out.extend_from_slice(b"\n");
            }
            out.extend_from_slice(b"endobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in offs {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        fs::write(path, out).unwrap();
    }

    #[test]
    fn parse_pages_accepts_lists_and_ranges() {
        assert_eq!(parse_pages("").unwrap(), vec![1]);
        assert_eq!(parse_pages("1-3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_pages("1,3").unwrap(), vec![1, 3]);
        assert_eq!(parse_pages(" 3 , 1-2 ").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_pages("2,2,1-2").unwrap(), vec![1, 2]);
        assert!(parse_pages("3-1").unwrap_err().contains("1-based"));
        assert!(parse_pages("0").unwrap_err().contains("1-based"));
        assert!(parse_pages("a").unwrap_err().contains("invalid page"));
        assert!(parse_pages("1-").unwrap_err().contains("invalid page range"));
        assert!(parse_pages("1,,2").unwrap_err().contains("empty page range"));
        assert!(parse_pages("1-60").unwrap_err().contains("limit is 50"));
    }

    #[test]
    fn dpi_clamps_to_default_and_max() {
        assert_eq!(dpi_value(None), 150);
        assert_eq!(dpi_value(Some(96)), 96);
        assert_eq!(dpi_value(Some(600)), 300);
        assert_eq!(dpi_value(Some(0)), 1);
    }

    #[test]
    fn png_dims_reads_ihdr() {
        let dir = temp_test_dir("pdfimg-dims");
        let f = dir.join("x.png");
        let mut bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        bytes.extend_from_slice(&123u32.to_be_bytes());
        bytes.extend_from_slice(&45u32.to_be_bytes());
        fs::write(&f, bytes).unwrap();
        assert_eq!(png_dims(&f), Some((123, 45)));
        fs::write(dir.join("junk.png"), b"not a png").unwrap();
        assert_eq!(png_dims(&dir.join("junk.png")), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn schema_shape() {
        let schema = PdfToImageTool::default().parameters();
        assert!(validate_args(&schema, &json!({ "pdf": "a.pdf" })).is_ok());
        assert!(validate_args(&schema, &json!({ "pdf": "a.pdf", "pages": "1-3", "dpi": 150, "output_dir": "o" })).is_ok());
        assert!(validate_args(&schema, &json!({})).unwrap_err().contains("pdf"));
        assert!(validate_args(&schema, &json!({ "pdf": "a.pdf", "dpi": "x" })).unwrap_err().contains("integer"));
        assert!(validate_args(&schema, &json!({ "pdf": "a.pdf", "bogus": 1 })).unwrap_err().contains("bogus"));
        assert_eq!(PdfToImageTool::default().policy(), PermissionPolicy::Skip);
        assert_eq!(PdfToImageTool::default().class(), ToolClass::Mutating);
    }

    #[test]
    fn input_validation_missing_extension_magic_and_bad_pages() {
        let wd = temp_test_dir("pdfimg-input");
        fs::create_dir_all(wd.join("in")).unwrap();
        fs::write(wd.join("in/notes.txt"), "plain").unwrap();
        fs::write(wd.join("in/fake.pdf"), "not a pdf at all").unwrap();
        write_pdf_fixture(&wd.join("in/real.pdf"));
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = PdfToImageTool::default();

        let r = tool.execute(&json!({ "pdf": "in/missing.pdf" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = tool.execute(&json!({ "pdf": "in/notes.txt" }), &ctx).unwrap();
        assert!(r.error.unwrap().message.contains("must end with .pdf"));
        let r = tool.execute(&json!({ "pdf": "in/fake.pdf" }), &ctx).unwrap();
        assert!(r.error.unwrap().message.contains("no %PDF magic"));
        let r = tool.execute(&json!({ "pdf": "in" }), &ctx).unwrap();
        assert!(r.error.unwrap().message.contains("is a directory"));
        let r = tool.execute(&json!({ "pdf": "in/real.pdf", "pages": "0" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn no_renderer_reports_an_explicit_error() {
        let wd = temp_test_dir("pdfimg-norenderer");
        fs::create_dir_all(wd.join("in")).unwrap();
        write_pdf_fixture(&wd.join("in/real.pdf"));
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = PdfToImageTool::with_renderers(
            Some(wd.join("no-pdftoppm")),
            wd.join("no-sips"),
            wd.join("no-qlmanage"),
        );
        let r = tool.execute(&json!({ "pdf": "in/real.pdf" }), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("brew install poppler"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn multi_page_without_pdftoppm_is_an_explicit_error() {
        let wd = temp_test_dir("pdfimg-pages-fallback");
        fs::create_dir_all(wd.join("in")).unwrap();
        write_pdf_fixture(&wd.join("in/real.pdf"));
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = PdfToImageTool::with_renderers(
            Some(wd.join("no-pdftoppm")),
            PathBuf::from(SIPS_BINARY),
            PathBuf::from(QLMANAGE_BINARY),
        );
        let r = tool.execute(&json!({ "pdf": "in/real.pdf", "pages": "1-2" }), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("beyond the first"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn live_pdftoppm_renders_requested_pages() {
        let Some(bin) = find_pdftoppm() else {
            eprintln!("no pdftoppm on this machine, skipping live pdf_to_image test");
            return;
        };
        let wd = temp_test_dir("pdfimg-live");
        fs::create_dir_all(wd.join("in")).unwrap();
        write_pdf_fixture(&wd.join("in/real.pdf"));
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = PdfToImageTool::with_renderers(Some(bin), PathBuf::from(SIPS_BINARY), PathBuf::from(QLMANAGE_BINARY));
        let r = tool
            .execute(&json!({ "pdf": "in/real.pdf", "pages": "1", "dpi": 72, "output_dir": "Harness/render" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["renderer"], "pdftoppm");
        let pages = data["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["page"], 1);
        let file = PathBuf::from(pages[0]["path"].as_str().unwrap());
        assert_eq!(file, wd.join("Harness/render/real-1.png"));
        assert!(file.is_file());
        assert_eq!(pages[0]["width"].as_u64().unwrap(), 200);
        assert_eq!(pages[0]["height"].as_u64().unwrap(), 200);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn live_sips_fallback_renders_page_one_with_a_note() {
        if !Path::new(SIPS_BINARY).is_file() {
            eprintln!("no /usr/bin/sips on this machine, skipping sips fallback test");
            return;
        }
        let wd = temp_test_dir("pdfimg-sips");
        fs::create_dir_all(wd.join("in")).unwrap();
        write_pdf_fixture(&wd.join("in/real.pdf"));
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = PdfToImageTool::with_renderers(
            Some(wd.join("no-pdftoppm")),
            PathBuf::from(SIPS_BINARY),
            wd.join("no-qlmanage"),
        );
        let r = tool.execute(&json!({ "pdf": "in/real.pdf" }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["renderer"], "sips");
        assert!(data["note"].as_str().unwrap().contains("only page 1"));
        let pages = data["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["page"], 1);
        assert!(PathBuf::from(pages[0]["path"].as_str().unwrap()).is_file());
        // default output_dir lives under Settings/tmp
        assert!(data["output_dir"]
            .as_str()
            .unwrap()
            .contains("Settings/tmp/pdf-"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
