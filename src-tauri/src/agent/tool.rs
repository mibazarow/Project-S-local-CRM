// Tool trait: one declaration drives both the LLM schema and the argument
// validator (SPEC 6.4 "инструмент = один источник истины").
pub use super::result::ToolResult;
use crate::error::Result;
use serde_json::{Map, Value};

#[cfg(test)]
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    ReadOnly,
    Mutating,
    Meta,
    ParallelSafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    Skip,
    Check,
    CheckConditional,
}

#[derive(Default)]
pub struct ToolCtx {
    pub project_id: Option<i64>,
    // Set for harness session runs: schedule tools bind new schedules to the
    // session that created them (SPEC 5.3 related/independent origin).
    pub session_id: Option<i64>,
    pub working_dir: Option<std::path::PathBuf>,
    // Shared app state handle: search opens its own DB connection through it,
    // fs/task tools borrow the UI connection from state.db.
    pub state: Option<std::sync::Arc<crate::state::AppState>>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    // Human-readable reason for the confirmation queue (SPEC 5.2). Check tools
    // may supply an explanation; CheckConditional tools decide per call: the
    // loop confirms only when this returns Some.
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        None
    }
    fn max_result_len(&self) -> usize {
        8_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult>;
}

// Minimal JSON-schema validator over the subset the catalog declares:
// object, required, types (string/number/boolean/array/object), enum,
// additionalProperties. Unknown properties are rejected by default
// (additionalProperties:false semantics) unless the schema opts in.
pub fn validate_args(schema: &Value, args: &Value) -> std::result::Result<(), String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    validate_object(schema, obj, "")
}

fn validate_object(schema: &Value, obj: &Map<String, Value>, path: &str) -> std::result::Result<(), String> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for req in required {
            if let Some(req) = req.as_str() {
                if !obj.contains_key(req) {
                    return Err(format!("missing required property \"{req}\""));
                }
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    let allow_extra = schema
        .get("additionalProperties")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allow_extra {
        for key in obj.keys() {
            let known = props.map(|p| p.contains_key(key)).unwrap_or(false);
            if !known {
                return Err(format!("unknown property \"{key}\""));
            }
        }
    }
    if let Some(props) = props {
        for (key, sub) in props {
            if let Some(val) = obj.get(key) {
                validate_value(sub, val, &format!("{path}{key}"))?;
            }
        }
    }
    Ok(())
}

fn validate_value(schema: &Value, val: &Value, path: &str) -> std::result::Result<(), String> {
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(val) {
            return Err(format!("{path}: value not in enum"));
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let obj = val
                .as_object()
                .ok_or_else(|| format!("{path}: expected object"))?;
            validate_object(schema, obj, &format!("{path}."))
        }
        Some("array") => {
            let arr = val
                .as_array()
                .ok_or_else(|| format!("{path}: expected array"))?;
            if let Some(items) = schema.get("items") {
                for (i, item) in arr.iter().enumerate() {
                    validate_value(items, item, &format!("{path}[{i}]"))?;
                }
            }
            Ok(())
        }
        Some("string") if !val.is_string() => Err(format!("{path}: expected string")),
        Some("number") if !val.is_number() => Err(format!("{path}: expected number")),
        Some("boolean") if !val.is_boolean() => Err(format!("{path}: expected boolean")),
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) struct EchoTool {
    pub policy: PermissionPolicy,
    pub class: ToolClass,
    pub max_len: usize,
}

#[cfg(test)]
impl EchoTool {
    pub(crate) fn new(policy: PermissionPolicy) -> Self {
        EchoTool {
            policy,
            class: ToolClass::ReadOnly,
            max_len: 8_000,
        }
    }
}

#[cfg(test)]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the given text back (test-only tool)"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        self.class
    }
    fn policy(&self) -> PermissionPolicy {
        self.policy
    }
    fn max_result_len(&self) -> usize {
        self.max_len
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        Ok(ToolResult::ok(json!({ "echo": args["text"] })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["finish", "stop"] },
                "count": { "type": "number" },
                "flag": { "type": "boolean" },
                "names": { "type": "array", "items": { "type": "string" } },
                "nested": {
                    "type": "object",
                    "properties": { "inner": { "type": "string" } },
                    "required": ["inner"]
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_valid_args() {
        let args = json!({
            "action": "finish",
            "count": 3,
            "flag": true,
            "names": ["a", "b"],
            "nested": { "inner": "x" }
        });
        assert!(validate_args(&schema(), &args).is_ok());
    }

    #[test]
    fn rejects_missing_required() {
        let err = validate_args(&schema(), &json!({ "count": 1 })).unwrap_err();
        assert!(err.contains("action"));
    }

    #[test]
    fn rejects_unknown_property() {
        let err = validate_args(&schema(), &json!({ "action": "stop", "bogus": 1 })).unwrap_err();
        assert!(err.contains("bogus"));
    }

    #[test]
    fn allows_extra_when_schema_opts_in() {
        let s = json!({ "type": "object", "additionalProperties": true });
        assert!(validate_args(&s, &json!({ "anything": 1 })).is_ok());
    }

    #[test]
    fn rejects_wrong_types() {
        assert!(validate_args(&schema(), &json!({ "action": "finish", "flag": "x" }))
            .unwrap_err()
            .contains("boolean"));
        assert!(validate_args(&schema(), &json!({ "action": "finish", "count": "x" }))
            .unwrap_err()
            .contains("number"));
        assert!(validate_args(&schema(), &json!({ "action": "finish", "names": [1] }))
            .unwrap_err()
            .contains("string"));
    }

    #[test]
    fn rejects_enum_violation() {
        let err = validate_args(&schema(), &json!({ "action": "explode" })).unwrap_err();
        assert!(err.contains("enum"));
    }

    #[test]
    fn rejects_nested_object_missing_required() {
        let err = validate_args(&schema(), &json!({ "action": "finish", "nested": {} })).unwrap_err();
        assert!(err.contains("inner"));
    }

    #[test]
    fn rejects_non_object_args() {
        assert!(validate_args(&schema(), &json!("hi")).is_err());
    }
}
