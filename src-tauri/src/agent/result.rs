// JSON envelope every tool call returns to the model (SPEC 6.4), plus the
// shared error-mapping and output-capping primitives that shape what tools
// put into that envelope (S5.3).
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorCode {
    InvalidArgs,
    NotFound,
    PermissionDenied,
    DeniedByUser,
    ExecutionError,
    Hidden,
    UnknownTool,
    LimitExceeded,
}

impl ToolErrorCode {
    // Reverse of as_str: lets the loop map a code string from a tool-returned
    // envelope back to the enum so error hints can be attached.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "invalid_args" => Some(ToolErrorCode::InvalidArgs),
            "not_found" => Some(ToolErrorCode::NotFound),
            "permission_denied" => Some(ToolErrorCode::PermissionDenied),
            "denied_by_user" => Some(ToolErrorCode::DeniedByUser),
            "execution_error" => Some(ToolErrorCode::ExecutionError),
            "hidden" => Some(ToolErrorCode::Hidden),
            "unknown_tool" => Some(ToolErrorCode::UnknownTool),
            "limit_exceeded" => Some(ToolErrorCode::LimitExceeded),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ToolErrorCode::InvalidArgs => "invalid_args",
            ToolErrorCode::NotFound => "not_found",
            ToolErrorCode::PermissionDenied => "permission_denied",
            ToolErrorCode::DeniedByUser => "denied_by_user",
            ToolErrorCode::ExecutionError => "execution_error",
            ToolErrorCode::Hidden => "hidden",
            ToolErrorCode::UnknownTool => "unknown_tool",
            ToolErrorCode::LimitExceeded => "limit_exceeded",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
}

impl ToolError {
    pub fn new(code: ToolErrorCode, message: impl Into<String>) -> Self {
        ToolError {
            code: code.as_str().to_string(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub ok: bool,
    pub data: Option<Value>,
    pub error: Option<ToolError>,
}

impl ToolResult {
    pub fn ok(data: Value) -> Self {
        ToolResult {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(code: ToolErrorCode, message: impl Into<String>) -> Self {
        ToolResult {
            ok: false,
            data: None,
            error: Some(ToolError::new(code, message)),
        }
    }

    // Enforces the tool's max_result_len on the serialized envelope: an
    // oversized result becomes a limit_exceeded error instead of flooding
    // the context window.
    pub fn capped(self, max_len: usize) -> ToolResult {
        let len = serde_json::to_string(&self).map(|s| s.len()).unwrap_or(usize::MAX);
        if len <= max_len {
            self
        } else {
            ToolResult::err(
                ToolErrorCode::LimitExceeded,
                format!("result of {len} chars exceeds the {max_len}-char limit; narrow the request"),
            )
        }
    }
}

// Maps an IO error on a path to the envelope: a missing file is not_found,
// any other fault is execution_error (S5.3, shared by the fs/edit tools).
pub(crate) fn io_fail(path: &Path, e: &std::io::Error) -> ToolResult {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolResult::err(ToolErrorCode::NotFound, format!("{} not found", path.display()))
    } else {
        ToolResult::err(ToolErrorCode::ExecutionError, format!("{}: {e}", path.display()))
    }
}

// Reads a pipe up to the cap plus one byte so truncation stays detectable
// while the child keeps producing (S5.3, shared by the process-spawning
// tools); returns the raw bytes and the truncation flag.
pub(crate) fn read_capped_bytes(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader)
        .take((cap as u64) + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > cap;
    buf.truncate(cap);
    (buf, truncated)
}

// The same cap applied to text output; the flag tells whether the child
// produced more than the cap.
pub(crate) fn read_capped(reader: impl Read, cap: usize) -> (String, bool) {
    let (buf, truncated) = read_capped_bytes(reader, cap);
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn io_fail_maps_missing_file_to_not_found() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file or directory");
        let r = io_fail(Path::new("/x/y.md"), &e);
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert_eq!(err.message, "/x/y.md not found");
    }

    #[test]
    fn io_fail_maps_other_io_faults_to_execution_error() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let r = io_fail(Path::new("/x/y.md"), &e);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert_eq!(err.message, "/x/y.md: permission denied");
    }

    #[test]
    fn read_capped_flags_only_output_beyond_the_cap() {
        let (text, truncated) = read_capped(std::io::Cursor::new("x".repeat(5)), 5);
        assert_eq!(text.len(), 5);
        assert!(!truncated);
        let (text, truncated) = read_capped(std::io::Cursor::new("x".repeat(6)), 5);
        assert_eq!(text.len(), 5);
        assert!(truncated);
    }

    #[test]
    fn read_capped_bytes_keeps_raw_bytes_and_flag() {
        let (bytes, truncated) = read_capped_bytes(std::io::Cursor::new([0u8, 155, 200]), 2);
        assert_eq!(bytes, vec![0u8, 155]);
        assert!(truncated);
        let (bytes, truncated) = read_capped_bytes(std::io::Cursor::new([0u8, 155]), 2);
        assert_eq!(bytes, vec![0u8, 155]);
        assert!(!truncated);
    }

    #[test]
    fn capped_keeps_small_results() {
        let r = ToolResult::ok(json!({"a": 1})).capped(1000);
        assert!(r.ok);
        assert_eq!(r.data, Some(json!({"a": 1})));
    }

    #[test]
    fn capped_turns_oversized_result_into_limit_exceeded() {
        let big = json!({"text": "x".repeat(500)});
        let r = ToolResult::ok(big).capped(100);
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "limit_exceeded");
        assert!(serde_json::to_string(&r).unwrap().len() < 200);
    }

    #[test]
    fn error_codes_are_snake_case() {
        assert_eq!(ToolErrorCode::DeniedByUser.as_str(), "denied_by_user");
        assert_eq!(ToolErrorCode::UnknownTool.as_str(), "unknown_tool");
    }
}
