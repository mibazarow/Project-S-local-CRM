// Markdown block model: the shared intermediate representation every writer
// (DOCX/PDF) renders from. parse_markdown turns the raw Markdown into a flat
// list of blocks; the small helpers recognise headings, lists, fenced code and
// pipe tables.
#[derive(Debug, PartialEq)]
pub(super) enum Block {
    Heading(usize, String),
    Paragraph(String),
    Bullets(Vec<String>),
    Numbered(Vec<String>),
    Code(Vec<String>),
    Table(Vec<Vec<String>>),
}

pub(super) fn parse_markdown(md: &str) -> Vec<Block> {
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
