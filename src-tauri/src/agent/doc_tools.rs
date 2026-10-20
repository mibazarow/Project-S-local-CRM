// Native document generation tools: create_document turns basic Markdown into
// DOCX/PDF/MD/TXT/HTML, create_spreadsheet writes XLSX/CSV from typed rows.
// All writers are pure Rust (docx-rs, rust_xlsxwriter, printpdf) — no
// pandoc/LibreOffice/Python. Project mode keeps every path inside the project
// root (Skip, like write_file); harness mode accepts absolute/~/working-dir-
// relative paths and skips confirmation only inside Harness/ and Projects/
// (CheckConditional, like fs_write). Existing files are only replaced with an
// explicit overwrite=true; content is capped at ~2 MiB.
use super::harness_tools::{canonicalish, in_default_territory, resolve_path};
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::projects;
use docx_rs::{Docx, PageMargin, Paragraph, Run, RunFonts, Section, TableCell, TableRow, Table};
use printpdf::{
    Color, FontId, Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point,
    Pt, Rect, Rgb, TextItem,
};
use rust_xlsxwriter::{Format, Workbook, XlsxError};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const SHEET_FORMATS: [&str; 2] = ["xlsx", "csv"];

// macOS system TTFs with Cyrillic coverage; the first one that parses and
// contains a Cyrillic glyph wins. .ttc collections are avoided on purpose.
const PDF_FONT_CANDIDATES: [&str; 5] = [
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Verdana.ttf",
    "/System/Library/Fonts/Supplemental/Tahoma.ttf",
];

// A4 in points and a 2 cm margin; printpdf measures from the bottom-left.
const PAGE_W: f32 = 595.0;
const PAGE_H: f32 = 842.0;
const MARGIN: f32 = 56.7;
const CONTENT_W: f32 = PAGE_W - 2.0 * MARGIN;

// ---------------------------------------------------------------------------
// Markdown block model
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Block {
    Heading(usize, String),
    Paragraph(String),
    Bullets(Vec<String>),
    Numbered(Vec<String>),
    Code(Vec<String>),
    Table(Vec<Vec<String>>),
}

fn parse_markdown(md: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut lines = md.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            flush_para(&mut para, &mut blocks);
            let mut code: Vec<String> = Vec::new();
            for l in lines.by_ref() {
                if l.trim().starts_with("```") {
                    break;
                }
                code.push(l.to_string());
            }
            blocks.push(Block::Code(code));
            continue;
        }
        if trimmed.is_empty() {
            flush_para(&mut para, &mut blocks);
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            flush_para(&mut para, &mut blocks);
            let mut rows = vec![split_table_row(trimmed)];
            while let Some(next) = lines.peek() {
                let nt = next.trim();
                if nt.starts_with('|') && nt.ends_with('|') {
                    rows.push(split_table_row(nt));
                    lines.next();
                } else {
                    break;
                }
            }
            rows.retain(|cells| {
                !cells
                    .iter()
                    .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
            });
            blocks.push(Block::Table(rows));
            continue;
        }
        if trimmed.starts_with('#') {
            flush_para(&mut para, &mut blocks);
            let level = trimmed.chars().take_while(|c| *c == '#').count().min(6);
            blocks.push(Block::Heading(level, trimmed[level..].trim().to_string()));
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            flush_para(&mut para, &mut blocks);
            let mut items = vec![trimmed[2..].trim().to_string()];
            while let Some(next) = lines.peek() {
                let nt = next.trim();
                if nt.starts_with("- ") || nt.starts_with("* ") {
                    items.push(nt[2..].trim().to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            blocks.push(Block::Bullets(items));
            continue;
        }
        if let Some(first) = numbered_item(trimmed) {
            flush_para(&mut para, &mut blocks);
            let mut items = vec![first];
            while let Some(next) = lines.peek() {
                if let Some(item) = numbered_item(next.trim()) {
                    items.push(item);
                    lines.next();
                } else {
                    break;
                }
            }
            blocks.push(Block::Numbered(items));
            continue;
        }
        para.push(trimmed.to_string());
    }
    flush_para(&mut para, &mut blocks);
    blocks
}

fn flush_para(para: &mut Vec<String>, blocks: &mut Vec<Block>) {
    if !para.is_empty() {
        blocks.push(Block::Paragraph(para.join(" ")));
        para.clear();
    }
}

fn numbered_item(line: &str) -> Option<String> {
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let mut chars = line[digits.len()..].chars();
    if !matches!(chars.next(), Some('.') | Some(')')) {
        return None;
    }
    if !matches!(chars.next(), Some(' ') | None) {
        return None;
    }
    Some(chars.as_str().trim().to_string())
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// DOCX (docx-rs)
// ---------------------------------------------------------------------------

// docx-rs pack() consumes the writer, so keep the buffer behind a shared
// handle to read the finished ZIP bytes back out.
#[derive(Clone, Default)]
struct SharedCursor(Rc<RefCell<std::io::Cursor<Vec<u8>>>>);

impl Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.borrow_mut().seek(pos)
    }
}

fn heading_size_halfpoints(level: usize) -> usize {
    match level {
        1 => 32,
        2 => 28,
        3 => 26,
        _ => 24,
    }
}

fn build_docx(title: Option<&str>, blocks: &[Block]) -> Result<Vec<u8>> {
    let mut section = Section::new().page_margin(
        PageMargin::new()
            .top(1134)
            .left(1134)
            .bottom(1134)
            .right(1134),
    );
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        section = section.add_paragraph(
            Paragraph::new()
                .add_run(Run::new().add_text(t).bold().size(48))
                .style("Title"),
        );
    }
    for block in blocks {
        match block {
            Block::Heading(level, text) => {
                section = section.add_paragraph(
                    Paragraph::new()
                        .add_run(
                            Run::new()
                                .add_text(text)
                                .bold()
                                .size(heading_size_halfpoints(*level)),
                        )
                        .style(&format!("Heading{level}")),
                );
            }
            Block::Paragraph(text) => {
                section = section
                    .add_paragraph(Paragraph::new().add_run(Run::new().add_text(text).size(22)));
            }
            Block::Bullets(items) => {
                for item in items {
                    section = section.add_paragraph(
                        Paragraph::new()
                            .add_run(Run::new().add_text(format!("• {item}")).size(22))
                            .indent(Some(420), None, None, None),
                    );
                }
            }
            Block::Numbered(items) => {
                for (i, item) in items.iter().enumerate() {
                    section = section.add_paragraph(
                        Paragraph::new()
                            .add_run(Run::new().add_text(format!("{}. {item}", i + 1)).size(22))
                            .indent(Some(420), None, None, None),
                    );
                }
            }
            Block::Code(lines) => {
                for line in lines {
                    section = section.add_paragraph(
                        Paragraph::new()
                            .add_run(
                                Run::new()
                                    .add_text(if line.is_empty() { " " } else { line })
                                    .size(20)
                                    .fonts(
                                        RunFonts::new()
                                            .ascii("Courier New")
                                            .hi_ansi("Courier New"),
                                    ),
                            )
                            .indent(Some(420), None, None, None),
                    );
                }
            }
            Block::Table(rows) => {
                let rows: Vec<TableRow> = rows
                    .iter()
                    .map(|cells| {
                        TableRow::new(
                            cells
                                .iter()
                                .map(|c| {
                                    TableCell::new().add_paragraph(
                                        Paragraph::new().add_run(Run::new().add_text(c).size(20)),
                                    )
                                })
                                .collect(),
                        )
                    })
                    .collect();
                section = section.add_table(Table::new(rows));
            }
        }
    }
    let sink = SharedCursor::default();
    Docx::new()
        .add_section(section)
        .pack(sink.clone())
        .map_err(|e| AppError::Invalid(format!("docx write failed: {e}")))?;
    let bytes = sink.0.borrow().get_ref().clone();
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// PDF (printpdf)
// ---------------------------------------------------------------------------

fn load_pdf_font() -> Result<ParsedFont> {
    for path in PDF_FONT_CANDIDATES {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let mut warnings = Vec::new();
        let Some(font) = ParsedFont::from_bytes(&bytes, 0, &mut warnings) else {
            continue;
        };
        // Cyrillic sanity check: the font must cover the letter "А".
        if font.codepoint_to_glyph.contains_key(&0x410) {
            return Ok(font);
        }
    }
    Err(AppError::Invalid(format!(
        "no Cyrillic-capable TTF font found on this system; tried: {}",
        PDF_FONT_CANDIDATES.join(", ")
    )))
}

fn text_width_pt(font: &ParsedFont, text: &str, size: f32) -> f32 {
    let upem = f32::from(font.units_per_em.max(1));
    let advance: f32 = text
        .chars()
        .map(|c| {
            font.codepoint_to_glyph
                .get(&(c as u32))
                .and_then(|gid| font.glyph_widths.get(gid))
                .copied()
                .unwrap_or((upem * 0.5) as u16) as f32
        })
        .sum();
    advance / upem * size
}

fn wrap_text(font: &ParsedFont, text: &str, size: f32, max_w: f32) -> Vec<String> {
    let space_w = text_width_pt(font, " ", size);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0.0f32;
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while text_width_pt(font, &word, size) > max_w {
            let mut take = 0usize;
            let mut chunk_w = 0.0f32;
            for ch in word.chars() {
                let mut buf = [0u8; 4];
                let cw = text_width_pt(font, ch.encode_utf8(&mut buf), size);
                if chunk_w + cw > max_w && take > 0 {
                    break;
                }
                chunk_w += cw;
                take += ch.len_utf8();
            }
            if take == 0 {
                take = 1;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_w = 0.0;
            }
            out.push(word[..take].to_string());
            word = word[take..].to_string();
        }
        let word_w = text_width_pt(font, &word, size);
        if !line.is_empty() && line_w + space_w + word_w > max_w {
            out.push(std::mem::take(&mut line));
            line_w = 0.0;
        }
        if !line.is_empty() {
            line.push(' ');
            line_w += space_w;
        }
        line.push_str(&word);
        line_w += word_w;
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

struct PdfWriter<'a> {
    font: &'a ParsedFont,
    font_id: FontId,
    pages: Vec<PdfPage>,
    ops: Vec<Op>,
    y: f32,
}

impl<'a> PdfWriter<'a> {
    fn new(font: &'a ParsedFont, font_id: FontId) -> Self {
        PdfWriter {
            font,
            font_id,
            pages: Vec::new(),
            ops: Vec::new(),
            y: PAGE_H - MARGIN,
        }
    }

    fn page_break(&mut self) {
        self.pages
            .push(PdfPage::new(Mm(210.0), Mm(297.0), std::mem::take(&mut self.ops)));
        self.y = PAGE_H - MARGIN;
    }

    fn ensure(&mut self, needed: f32) {
        if self.y - needed < MARGIN {
            self.page_break();
        }
    }

    fn put_line(&mut self, x: f32, size: f32, line_h: f32, text: &str) {
        self.ensure(line_h);
        self.ops.extend([
            Op::StartTextSection,
            Op::SetFont {
                font: PdfFontHandle::External(self.font_id.clone()),
                size: Pt(size),
            },
            Op::SetTextCursor {
                pos: Point {
                    x: Pt(x),
                    y: Pt(self.y),
                },
            },
            Op::ShowText {
                items: vec![TextItem::Text(text.to_string())],
            },
            Op::EndTextSection,
        ]);
        self.y -= line_h;
    }

    fn rule(&mut self, y: f32) {
        self.ops.extend([
            Op::SetFillColor {
                col: Color::Rgb(Rgb::new(0.75, 0.75, 0.75, None)),
            },
            Op::DrawRectangle {
                rectangle: Rect::from_xywh(Pt(MARGIN), Pt(y), Pt(CONTENT_W), Pt(0.6)),
            },
        ]);
    }

    fn write_blocks(&mut self, blocks: &[Block]) {
        for block in blocks {
            match block {
                Block::Heading(level, text) => {
                    let size = match level {
                        1 => 16.0,
                        2 => 14.0,
                        3 => 12.5,
                        _ => 11.5,
                    };
                    for line in wrap_text(self.font, text, size, CONTENT_W) {
                        self.put_line(MARGIN, size, size * 1.4, &line);
                    }
                    self.y -= 4.0;
                }
                Block::Paragraph(text) => {
                    for line in wrap_text(self.font, text, 11.0, CONTENT_W) {
                        self.put_line(MARGIN, 11.0, 15.0, &line);
                    }
                    self.y -= 4.0;
                }
                Block::Bullets(items) => {
                    for item in items {
                        let text = format!("• {item}");
                        for line in wrap_text(self.font, &text, 11.0, CONTENT_W - 14.0) {
                            self.put_line(MARGIN + 14.0, 11.0, 15.0, &line);
                        }
                    }
                    self.y -= 4.0;
                }
                Block::Numbered(items) => {
                    for (i, item) in items.iter().enumerate() {
                        let text = format!("{}. {item}", i + 1);
                        for line in wrap_text(self.font, &text, 11.0, CONTENT_W - 14.0) {
                            self.put_line(MARGIN + 14.0, 11.0, 15.0, &line);
                        }
                    }
                    self.y -= 4.0;
                }
                Block::Code(lines) => {
                    for line in lines {
                        for wrapped in wrap_text(self.font, line, 9.0, CONTENT_W - 20.0) {
                            self.put_line(MARGIN + 20.0, 9.0, 12.0, &wrapped);
                        }
                    }
                    self.y -= 4.0;
                }
                Block::Table(rows) => {
                    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                    if cols == 0 {
                        continue;
                    }
                    let mut col_w = vec![12.0f32; cols];
                    for row in rows {
                        for (i, cell) in row.iter().enumerate() {
                            col_w[i] = col_w[i].max(text_width_pt(self.font, cell, 10.0) + 8.0);
                        }
                    }
                    let total: f32 = col_w.iter().sum();
                    let widths: Vec<f32> = col_w.iter().map(|x| x / total * CONTENT_W).collect();
                    let row_h = 16.0f32;
                    for row in rows {
                        self.ensure(row_h);
                        let baseline = self.y - 11.0;
                        let mut x = MARGIN + 4.0;
                        for (ci, cell) in row.iter().enumerate() {
                            self.ops.extend([
                                Op::StartTextSection,
                                Op::SetFont {
                                    font: PdfFontHandle::External(self.font_id.clone()),
                                    size: Pt(10.0),
                                },
                                Op::SetTextCursor {
                                    pos: Point {
                                        x: Pt(x),
                                        y: Pt(baseline),
                                    },
                                },
                                Op::ShowText {
                                    items: vec![TextItem::Text(cell.clone())],
                                },
                                Op::EndTextSection,
                            ]);
                            x += widths[ci];
                        }
                        self.rule(self.y - row_h + 3.0);
                        self.y -= row_h;
                    }
                    self.y -= 8.0;
                }
            }
        }
    }

    fn finish(mut self, doc: &mut PdfDocument) -> Vec<u8> {
        let mut pages = std::mem::take(&mut self.pages);
        if !self.ops.is_empty() {
            pages.push(PdfPage::new(Mm(210.0), Mm(297.0), std::mem::take(&mut self.ops)));
        }
        if pages.is_empty() {
            pages.push(PdfPage::new(Mm(210.0), Mm(297.0), Vec::new()));
        }
        doc.with_pages(pages);
        doc.save(&PdfSaveOptions::default(), &mut Vec::new())
    }
}

fn build_pdf(title: Option<&str>, blocks: &[Block]) -> Result<Vec<u8>> {
    let font = load_pdf_font()?;
    let doc_title = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Document");
    let mut doc = PdfDocument::new(doc_title);
    let font_id = doc.add_font(&font);
    let mut writer = PdfWriter::new(&font, font_id);
    if title.map(str::trim).filter(|t| !t.is_empty()).is_some() {
        for line in wrap_text(&font, doc_title, 20.0, CONTENT_W) {
            writer.put_line(MARGIN, 20.0, 26.0, &line);
        }
        writer.y -= 6.0;
    }
    writer.write_blocks(blocks);
    Ok(writer.finish(&mut doc))
}

// ---------------------------------------------------------------------------
// Plain text / HTML
// ---------------------------------------------------------------------------

fn plain_with_title(title: Option<&str>, markdown: &str, as_markdown: bool) -> String {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    match title {
        Some(t) if as_markdown => format!("# {t}\n\n{markdown}"),
        Some(t) => format!("{t}\n\n{markdown}"),
        None => markdown.to_string(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn html_wrapper(title: Option<&str>, markdown: &str) -> String {
    let t = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Document");
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n</head>\n<body>\n<h1>{}</h1>\n<pre>{}</pre>\n</body>\n</html>\n",
        html_escape(t),
        html_escape(t),
        html_escape(markdown)
    )
}

// ---------------------------------------------------------------------------
// XLSX / CSV
// ---------------------------------------------------------------------------

fn xlsx_fail(e: XlsxError) -> AppError {
    AppError::Invalid(format!("xlsx error: {e}"))
}

fn build_xlsx(sheets: &[Value]) -> Result<Vec<u8>> {
    let mut wb = Workbook::new();
    let bold = Format::new().set_bold();
    for sheet in sheets {
        let name = sheet["name"].as_str().unwrap_or_default();
        let rows = sheet["rows"].as_array().cloned().unwrap_or_default();
        let ws = wb.add_worksheet();
        ws.set_name(name).map_err(xlsx_fail)?;
        let mut col_widths: Vec<f64> = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            let cells = row.as_array().cloned().unwrap_or_default();
            for (c, cell) in cells.iter().enumerate() {
                col_widths.resize(c + 1, 10.0);
                let text_len = cell
                    .as_str()
                    .map(|s| s.chars().count())
                    .unwrap_or_else(|| cell.to_string().chars().count());
                col_widths[c] = col_widths[c].max((text_len + 2) as f64);
                let (r, c) = (r as u32, c as u16);
                match cell {
                    Value::Null => {
                        ws.write_blank(r, c, &Format::new()).map_err(xlsx_fail)?;
                    }
                    Value::String(s) => {
                        if r == 0 {
                            ws.write_string_with_format(r, c, s, &bold)
                        } else {
                            ws.write_string(r, c, s)
                        }
                        .map_err(xlsx_fail)?;
                    }
                    Value::Number(n) => {
                        ws.write_number(r, c, n.as_f64().unwrap_or(0.0))
                            .map_err(xlsx_fail)?;
                    }
                    other => {
                        ws.write_string(r, c, other.to_string()).map_err(xlsx_fail)?;
                    }
                }
            }
        }
        for (c, w) in col_widths.iter().enumerate() {
            ws.set_column_width(c as u16, w.clamp(10.0, 50.0))
                .map_err(xlsx_fail)?;
        }
    }
    wb.save_to_buffer().map_err(xlsx_fail)
}

fn csv_quote(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => csv_quote(s),
        Value::Number(n) => n.to_string(),
        other => csv_quote(&other.to_string()),
    }
}

fn build_csv(sheet: &Value) -> Vec<u8> {
    let rows = sheet["rows"].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    for row in rows {
        let cells = row.as_array().cloned().unwrap_or_default();
        out.push_str(
            &cells
                .iter()
                .map(csv_cell)
                .collect::<Vec<String>>()
                .join(","),
        );
        out.push('\n');
    }
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Shared path resolution and write helpers
// ---------------------------------------------------------------------------

// (label shown back to the model, absolute target path).
fn resolve_target(args: &Value, ctx: &ToolCtx, wd: &Path) -> Result<(String, PathBuf)> {
    let raw = args["path"].as_str().unwrap_or_default();
    if raw.trim().is_empty() {
        return Err(AppError::Invalid("path is empty".into()));
    }
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            let joined = crate::path_policy::join_rel(&root, raw)?;
            let name = joined
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            // An existing target (also via a lenient dash/space/case/NFD
            // variant) keeps its real path; a new one keeps the requested
            // name under leniently resolved parent folders.
            let joined = match super::harness_tools::lenient_existing(&joined) {
                Ok(p) => p,
                Err(e) if e.to_string().contains("ambiguous") => return Err(e),
                Err(_) => crate::path_policy::lenient_parent(&joined)?,
            };
            if !canonicalish(&joined).starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok((raw.to_owned(), joined))
        }
        None => {
            let path = resolve_path(raw, wd);
            let name = path
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            let path = match super::harness_tools::lenient_existing(&path) {
                Ok(p) => p,
                Err(e) if e.to_string().contains("ambiguous") => return Err(e),
                Err(_) => crate::path_policy::lenient_parent(&path)?,
            };
            Ok((path.display().to_string(), path))
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

// Directory targets and existing files are refused unless overwrite=true.
fn guard_target(path: &Path, args: &Value) -> std::result::Result<(), ToolResult> {
    if path.is_dir() {
        return Err(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!("{} is a directory", path.display()),
        ));
    }
    if path.exists() && !args.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
        return Err(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "{} already exists; pass overwrite=true to replace it",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// Parent folders are created on demand; the file itself is written through a
// temp file + rename so a failure never leaves a half-written document.
fn write_atomic(path: &Path, content: &[u8]) -> Result<usize> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_owned());
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.doc-tmp-{}", nanos()));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(content.len())
}

fn content_cap_error(len: usize) -> ToolResult {
    ToolResult::err(
        ToolErrorCode::InvalidArgs,
        format!("content is {len} bytes, over the {MAX_CONTENT_BYTES}-byte cap"),
    )
}

// ---------------------------------------------------------------------------
// create_document
// ---------------------------------------------------------------------------

pub struct CreateDocumentTool {
    harness: bool,
}

impl CreateDocumentTool {
    pub fn project() -> Self {
        CreateDocumentTool { harness: false }
    }
    pub fn harness() -> Self {
        CreateDocumentTool { harness: true }
    }
}

impl Tool for CreateDocumentTool {
    fn name(&self) -> &str {
        "create_document"
    }
    fn description(&self) -> &str {
        "Create a document from Markdown with native Rust writers (no external tools). format: docx|pdf|md|txt|html, and the path extension must match it. Markdown supports headings (#..######), paragraphs, bullet (-, *) and numbered lists, fenced code blocks and pipe tables. DOCX gets a title, sized headings, indented lists, monospace code and bordered tables; PDF uses a Cyrillic-capable macOS system font with word wrap and multi-page pagination; md/txt write the Markdown as-is; html wraps it in a minimal escaped page. Existing files are only replaced with overwrite=true. path is project-relative (project agent) or absolute/~/working-dir-relative (harness agent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "format": { "type": "string", "enum": ["docx", "pdf", "md", "txt", "html"] },
                "title": { "type": "string" },
                "markdown": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["path", "format", "markdown"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        if self.harness {
            PermissionPolicy::CheckConditional
        } else {
            PermissionPolicy::Skip
        }
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        if !self.harness {
            return None;
        }
        let wd = ctx.working_dir.as_deref()?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd))
            .then(|| "создание файла вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let format = args["format"].as_str().unwrap_or_default();
        let markdown = args["markdown"].as_str().unwrap_or_default();
        let title = args.get("title").and_then(Value::as_str);
        if markdown.len() > MAX_CONTENT_BYTES {
            return Ok(content_cap_error(markdown.len()));
        }
        let (label, path) = match resolve_target(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let ext = extension(&path);
        if ext != format {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("path extension \"{ext}\" does not match format \"{format}\""),
            ));
        }
        if let Err(r) = guard_target(&path, args) {
            return Ok(r);
        }
        let blocks = parse_markdown(markdown);
        let bytes = match format {
            "docx" => build_docx(title, &blocks)?,
            "pdf" => build_pdf(title, &blocks)?,
            "md" => plain_with_title(title, markdown, true).into_bytes(),
            "txt" => plain_with_title(title, markdown, false).into_bytes(),
            "html" => html_wrapper(title, markdown).into_bytes(),
            other => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("unsupported format \"{other}\""),
                ))
            }
        };
        match write_atomic(&path, &bytes) {
            Ok(written) => Ok(ToolResult::ok(json!({
                "path": label,
                "format": format,
                "bytes": written,
            }))),
            Err(e) => Ok(envelope::<()>(Err(e))),
        }
    }
}

// ---------------------------------------------------------------------------
// create_spreadsheet
// ---------------------------------------------------------------------------

pub struct CreateSpreadsheetTool {
    harness: bool,
}

impl CreateSpreadsheetTool {
    pub fn project() -> Self {
        CreateSpreadsheetTool { harness: false }
    }
    pub fn harness() -> Self {
        CreateSpreadsheetTool { harness: true }
    }
}

impl Tool for CreateSpreadsheetTool {
    fn name(&self) -> &str {
        "create_spreadsheet"
    }
    fn description(&self) -> &str {
        "Create a spreadsheet with native Rust writers (no external tools). The path extension picks the format: .xlsx (one sheet per entry, first row bold, auto column widths, numbers stored as numbers) or .csv (exactly one sheet, RFC-style quoting). Cell values are string|number|boolean|null. Existing files are only replaced with overwrite=true. path is project-relative (project agent) or absolute/~/working-dir-relative (harness agent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "sheets": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "rows": {
                                "type": "array",
                                "items": {
                                    "type": "array",
                                    "items": { "type": ["string", "number", "boolean", "null"] }
                                }
                            }
                        },
                        "required": ["name", "rows"],
                        "additionalProperties": false
                    }
                },
                "overwrite": { "type": "boolean" }
            },
            "required": ["path", "sheets"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        if self.harness {
            PermissionPolicy::CheckConditional
        } else {
            PermissionPolicy::Skip
        }
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        if !self.harness {
            return None;
        }
        let wd = ctx.working_dir.as_deref()?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd))
            .then(|| "создание файла вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let (label, path) = match resolve_target(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let format = extension(&path);
        if !SHEET_FORMATS.contains(&format.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "path must end with .xlsx or .csv",
            ));
        }
        let sheets = match args["sheets"].as_array() {
            Some(list) if !list.is_empty() => list.clone(),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "sheets must be a non-empty array",
                ))
            }
        };
        let json_len = serde_json::to_string(&sheets).map(|s| s.len()).unwrap_or(usize::MAX);
        if json_len > MAX_CONTENT_BYTES {
            return Ok(content_cap_error(json_len));
        }
        if let Err(r) = guard_target(&path, args) {
            return Ok(r);
        }
        let bytes = if format == "csv" {
            if sheets.len() != 1 {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "CSV supports exactly one sheet",
                ));
            }
            build_csv(&sheets[0])
        } else {
            build_xlsx(&sheets)?
        };
        match write_atomic(&path, &bytes) {
            Ok(written) => Ok(ToolResult::ok(json!({
                "path": label,
                "format": format,
                "bytes": written,
            }))),
            Err(e) => Ok(envelope::<()>(Err(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use crate::state::AppState;
    use std::io::Read as _;
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
