use crate::error::{AppError, Result};
use std::{fs, path::Path};

pub fn extract(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    // mupdf-backed: malformed input comes back as Err, pages without text yield no output.
    pdf_extract::extract_text_from_mem(&bytes)
        .map_err(|e| AppError::Invalid(format!("cannot read pdf: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    // Minimal single-page PDF with correct xref offsets.
    fn pdf_bytes(text: &str) -> Vec<u8> {
        let content = format!("BT /F1 12 Tf 20 180 Td ({text}) Tj ET");
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()).into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(obj);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn tiny_valid_pdf_extracts_text() {
        let dir = temp_test_dir("extract-pdf-ok");
        let path = dir.join("t.pdf");
        fs::write(&path, pdf_bytes("Hello PDF")).unwrap();
        let text = extract(&path).unwrap();
        assert!(text.contains("Hello PDF"), "got: {text:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_pdf_returns_err_without_panic() {
        let dir = temp_test_dir("extract-pdf-bad");
        let path = dir.join("bad.pdf");
        fs::write(&path, b"%PDF-1.7\nthis is not a pdf body\n%%EOF").unwrap();
        assert!(extract(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
