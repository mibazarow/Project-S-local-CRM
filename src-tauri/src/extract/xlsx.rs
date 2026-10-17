use crate::error::{AppError, Result};
use crate::extract::{open_zip, read_zip_entry, resolve_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::path::Path;

pub fn extract(path: &Path) -> Result<String> {
    let mut zip = open_zip(path)?;
    let shared = read_zip_entry(&mut zip, "xl/sharedStrings.xml")?
        .map(|xml| parse_shared_strings(&xml))
        .transpose()?
        .unwrap_or_default();

    let mut sheets: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .map(str::to_owned)
        .collect();
    if sheets.is_empty() {
        return Err(AppError::Invalid("xlsx has no worksheets".into()));
    }
    sheets.sort_by_key(|n| sheet_number(n));

    let mut out = Vec::new();
    for name in &sheets {
        let xml = read_zip_entry(&mut zip, name)?
            .ok_or_else(|| AppError::Invalid(format!("cannot read {name}")))?;
        out.push(parse_sheet(&xml, &shared)?);
    }
    Ok(out.join("\n"))
}

fn sheet_number(name: &str) -> u32 {
    name["xl/worksheets/sheet".len()..name.len() - ".xml".len()]
        .parse()
        .unwrap_or(0)
}

fn parse_shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let xml = utf8(xml)?;
    let mut reader = Reader::from_str(xml);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_si = false;
    let mut in_t = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| AppError::Invalid(format!("malformed sharedStrings.xml: {e}")))?;
        match event {
            Event::Start(e) => match e.local_name().as_ref() {
                "si" => {
                    in_si = true;
                    current.clear();
                }
                "t" if in_si => in_t = true,
                _ => {}
            },
            Event::Text(e) if in_t => current.push_str(&e),
            Event::CData(e) if in_t => current.push_str(&e),
            Event::GeneralRef(r) if in_t => {
                if let Some(text) = resolve_entity(&r.into_inner()) {
                    current.push_str(&text);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_t = false,
                "si" => {
                    strings.push(std::mem::take(&mut current));
                    in_si = false;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(strings)
}

fn parse_sheet(xml: &[u8], shared: &[String]) -> Result<String> {
    let xml = utf8(xml)?;
    let mut reader = Reader::from_str(xml);
    let mut rows: Vec<String> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell_type = String::new();
    let mut value = String::new();
    let mut in_value = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| AppError::Invalid(format!("malformed worksheet xml: {e}")))?;
        match event {
            Event::Start(e) => {
                if e.local_name().as_ref() == "c" {
                    cell_type = attr(&e, "t");
                    value.clear();
                }
                in_value = matches!(e.local_name().as_ref(), "v" | "t");
            }
            Event::Empty(e) if e.local_name().as_ref() == "c" => row.push(String::new()),
            Event::Text(e) if in_value => value.push_str(&e),
            Event::CData(e) if in_value => value.push_str(&e),
            Event::GeneralRef(r) if in_value => {
                if let Some(text) = resolve_entity(&r.into_inner()) {
                    value.push_str(&text);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                "v" | "t" => in_value = false,
                "c" => {
                    let cell = if cell_type == "s" {
                        let index: usize = value.trim().parse().unwrap_or(usize::MAX);
                        shared.get(index).cloned().unwrap_or_default()
                    } else {
                        std::mem::take(&mut value)
                    };
                    row.push(cell);
                }
                "row" => {
                    if row.iter().any(|c| !c.is_empty()) {
                        rows.push(row.join("\t"));
                    }
                    row.clear();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(rows.join("\n"))
}

fn attr(e: &BytesStart, key: &str) -> String {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| a.value.into_owned())
        .unwrap_or_default()
}

fn utf8(xml: &[u8]) -> Result<&str> {
    std::str::from_utf8(xml).map_err(|_| AppError::Invalid("worksheet xml is not valid UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::io::Write as _;

    fn xlsx_bytes(shared_strings: &str, sheet: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        let entries = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            ("xl/sharedStrings.xml", shared_strings),
            ("xl/worksheets/sheet1.xml", sheet),
        ];
        for (name, content) in entries {
            zip.start_file(name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn shared_inline_and_numeric_cells_are_extracted() {
        let shared = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2" uniqueCount="2"><si><t>Name</t></si><si><r><t>При</t></r><r><t>вет</t></r></si></sst>"#;
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>Value</t></is></c></row>
<row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>42</v></c></row>
<row r="3"><c r="A3" t="s"><v>99</v></c></row>
</sheetData></worksheet>"#;
        let dir = temp_test_dir("extract-xlsx");
        let path = dir.join("t.xlsx");
        std::fs::write(&path, xlsx_bytes(shared, sheet)).unwrap();
        assert_eq!(extract(&path).unwrap(), "Name\tValue\nПривет\t42");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_shared_strings_still_parses_inline_cells() {
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>only</t></is></c></row></sheetData></worksheet>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
        zip.write_all(sheet.as_bytes()).unwrap();
        let dir = temp_test_dir("extract-xlsx-noshared");
        let path = dir.join("t.xlsx");
        std::fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
        assert_eq!(extract(&path).unwrap(), "only");
        fs::remove_dir_all(&dir).unwrap();
    }
}
