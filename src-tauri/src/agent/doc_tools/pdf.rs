// PDF writer (printpdf): lays the Markdown blocks out on A4 pages using a
// Cyrillic-capable macOS system TTF, with word wrap and multi-page pagination.
use super::markdown::Block;
use crate::error::{AppError, Result};
use printpdf::{
    Color, FontId, Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point,
    Pt, Rect, Rgb, TextItem,
};
use std::fs;

// macOS system TTFs with Cyrillic coverage; the first one that parses and
// contains a Cyrillic glyph wins. .ttc collections are avoided on purpose.
pub(super) const PDF_FONT_CANDIDATES: [&str; 5] = [
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

pub(super) fn build_pdf(title: Option<&str>, blocks: &[Block]) -> Result<Vec<u8>> {
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
