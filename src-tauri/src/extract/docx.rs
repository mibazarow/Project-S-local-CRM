use crate::error::{AppError, Result};
use crate::extract::{open_zip, read_zip_entry, resolve_entity};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::path::Path;

pub fn extract(path: &Path) -> Result<String> {
    let mut zip = open_zip(path)?;
    let xml = read_zip_entry(&mut zip, "word/document.xml")?
        .ok_or_else(|| AppError::Invalid("docx has no word/document.xml".into()))?;
    let xml = std::str::from_utf8(&xml)
        .map_err(|_| AppError::Invalid("word/document.xml is not valid UTF-8".into()))?;

    let mut reader = Reader::from_str(xml);
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_text = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| AppError::Invalid(format!("malformed document.xml: {e}")))?;
        match event {
            Event::Start(e) => match e.local_name().as_ref() {
                "t" => in_text = true,
                "tab" => current.push('\t'),
                "br" | "cr" => current.push('\n'),
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                "tab" => current.push('\t'),
                "br" | "cr" => current.push('\n'),
                _ => {}
            },
            Event::Text(e) if in_text => current.push_str(&e),
            Event::CData(e) if in_text => current.push_str(&e),
            Event::GeneralRef(r) if in_text => {
                if let Some(text) = resolve_entity(&r.into_inner()) {
                    current.push_str(&text);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => paragraphs.push(std::mem::take(&mut current)),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    // Table cells hold their own w:p paragraphs, so their text is already included.
    Ok(paragraphs.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::io::Write as _;

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

    #[test]
    fn paragraphs_runs_and_table_cells_are_extracted() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space="preserve"> &amp; </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
<w:p><w:r><w:t>Second &lt;para&gt;</w:t></w:r></w:p>
<w:p><w:r><w:t>Tab&#9;sep</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cell B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#;
        let dir = temp_test_dir("extract-docx");
        let path = dir.join("t.docx");
        fs::write(&path, zip_bytes(&[("word/document.xml", xml)])).unwrap();
        assert_eq!(
            extract(&path).unwrap(),
            "Hello & world\nSecond <para>\nTab\tsep\nCell A\nCell B"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_document_xml_is_err() {
        let dir = temp_test_dir("extract-docx-missing");
        let path = dir.join("t.docx");
        fs::write(
            &path,
            zip_bytes(&[("word/other.xml", "<w:document/>")]),
        )
        .unwrap();
        assert!(extract(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
