mod docx;
mod pdf;
mod xlsx;

use crate::error::{AppError, Result};
use std::fs;
use std::io::Read;
use std::path::Path;

// Cap on decompressed bytes per zip entry (docx/xlsx).
pub(crate) const MAX_ENTRY_BYTES: u64 = 50 * 1024 * 1024;

pub fn is_supported(path: &Path) -> bool {
    kind(path).is_some()
}

pub fn extract_text(path: &Path, max_chars: usize) -> Result<String> {
    let text = match kind(path) {
        Some("text") => read_as_text(path)?,
        Some("docx") => docx::extract(path)?,
        Some("xlsx") => xlsx::extract(path)?,
        Some("pdf") => pdf::extract(path)?,
        _ => return Err(unsupported(path)),
    };
    Ok(truncate_chars(&text, max_chars))
}

pub(crate) fn kind(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "csv" | "json" | "xml" | "html" | "htm" => Some("text"),
        "docx" => Some("docx"),
        "xlsx" | "xlsm" => Some("xlsx"),
        "pdf" => Some("pdf"),
        _ => None,
    }
}

fn unsupported(path: &Path) -> AppError {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    AppError::Invalid(format!("unsupported_type: {name}"))
}

fn read_as_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        return Err(unsupported(path));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((i, _)) => text[..i].to_owned(),
        None => text.to_owned(),
    }
}

// quick-xml 0.42 reports entity references as separate events; resolve the XML predefined set plus numeric refs.
pub(crate) fn resolve_entity(name: &str) -> Option<String> {
    match name {
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "amp" => Some("&".into()),
        "quot" => Some("\"".into()),
        "apos" => Some("'".into()),
        _ => name
            .strip_prefix('#')
            .map(|num| match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => num.parse::<u32>(),
            })
            .and_then(|v| v.ok())
            .and_then(char::from_u32)
            .map(String::from),
    }
}

pub(crate) fn open_zip(path: &Path) -> Result<zip::ZipArchive<fs::File>> {
    let file = fs::File::open(path)?;
    zip::ZipArchive::new(file).map_err(|e| AppError::Invalid(format!("not a valid zip: {e}")))
}

pub(crate) fn read_zip_entry<C: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<C>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let entry = match zip.by_name(name) {
        Ok(entry) => Some(entry),
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(e) => return Err(AppError::Invalid(format!("broken zip entry {name}: {e}"))),
    };
    let Some(mut entry) = entry else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    entry
        .by_ref()
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| AppError::Invalid(format!("cannot read zip entry {name}: {e}")))?;
    if buf.len() as u64 > MAX_ENTRY_BYTES {
        return Err(AppError::Invalid(format!(
            "zip entry {name} exceeds {} MB decompressed",
            MAX_ENTRY_BYTES / (1024 * 1024)
        )));
    }
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::path::PathBuf;

    fn write_tmp(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn text_files_read_and_truncate_on_char_boundary() {
        let dir = temp_test_dir("extract-text");
        let md = write_tmp(&dir, "a.md", "# заголовок\nbody".as_bytes());
        assert_eq!(extract_text(&md, 1000).unwrap(), "# заголовок\nbody");
        let cut = extract_text(&md, 10).unwrap();
        assert_eq!(cut.chars().count(), 10);
        assert_eq!(cut, "# заголово");
        let txt = write_tmp(&dir, "b.txt", "abc".as_bytes());
        assert_eq!(extract_text(&txt, 2).unwrap(), "ab");
        let empty = write_tmp(&dir, "empty.txt", b"");
        assert_eq!(extract_text(&empty, 10).unwrap(), "");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsupported_extension_and_binary_txt_are_rejected() {
        let dir = temp_test_dir("extract-unsupported");
        let doc = write_tmp(&dir, "legacy.doc", b"old format");
        let err = extract_text(&doc, 100).unwrap_err();
        assert!(matches!(err, AppError::Invalid(_)));
        assert!(err.to_string().contains("unsupported_type"));
        assert!(!is_supported(&doc));
        let bin = write_tmp(&dir, "data.txt", b"ok\x00\x01\xff");
        assert!(extract_text(&bin, 100)
            .unwrap_err()
            .to_string()
            .contains("unsupported_type"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn is_supported_is_case_insensitive() {
        assert!(is_supported(Path::new("a.DOCX")));
        assert!(is_supported(Path::new("b.XLSM")));
        assert!(is_supported(Path::new("c.Pdf")));
        assert!(is_supported(Path::new("d.Markdown")));
        assert!(!is_supported(Path::new("e.doc")));
        assert!(!is_supported(Path::new("noext")));
    }

    #[test]
    fn malformed_zip_returns_err_without_panic() {
        let dir = temp_test_dir("extract-badzip");
        for name in ["broken.docx", "broken.xlsx"] {
            let p = write_tmp(&dir, name, b"PK\x03\x04this is not a real zip archive");
            assert!(extract_text(&p, 100).is_err(), "expected Err for {name}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
