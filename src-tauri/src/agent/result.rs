// JSON envelope every tool call returns to the model (SPEC 6.4).
use serde::Serialize;
use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
