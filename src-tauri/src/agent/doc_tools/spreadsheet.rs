// XLSX / CSV writers: create_spreadsheet turns typed rows into a workbook
// (rust_xlsxwriter) or an RFC-style CSV.
use crate::error::{AppError, Result};
use rust_xlsxwriter::{Format, Workbook, XlsxError};
use serde_json::Value;

pub(super) const SHEET_FORMATS: [&str; 2] = ["xlsx", "csv"];

fn xlsx_fail(e: XlsxError) -> AppError {
    AppError::Invalid(format!("xlsx error: {e}"))
}

pub(super) fn build_xlsx(sheets: &[Value]) -> Result<Vec<u8>> {
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

pub(super) fn build_csv(sheet: &Value) -> Vec<u8> {
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
