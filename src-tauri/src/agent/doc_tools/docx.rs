// DOCX writer (docx-rs): renders the Markdown blocks into an OOXML package.
use super::markdown::Block;
use crate::error::{AppError, Result};
use docx_rs::{Docx, PageMargin, Paragraph, Run, RunFonts, Section, TableCell, TableRow, Table};
use std::cell::RefCell;
use std::io::{Seek, SeekFrom, Write};
use std::rc::Rc;

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

pub(super) fn build_docx(title: Option<&str>, blocks: &[Block]) -> Result<Vec<u8>> {
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
