// Tool trait: one declaration drives both the LLM schema and the argument
// validator (SPEC 6.4 "инструмент = один источник истины").
pub use super::result::ToolResult;
use crate::error::Result;
use serde_json::{json, Map, Value};

#[allow(dead_code)] // taxonomy read by cfg(test) invariants/catalog checks; ParallelSafe reserved for the parallel scheduler
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

#[derive(Clone, Default)]
pub struct ToolCtx {
    pub project_id: Option<i64>,
    // Set for harness session runs: schedule tools bind new schedules to the
    // session that created them (SPEC 5.3 related/independent origin).
    pub session_id: Option<i64>,
    // Chat the run is writing into: send_file appends its agent attachment to
    // this chat (SPEC 6.1 agent-initiated attachments). Set for every chat run.
    pub chat_id: Option<i64>,
    pub working_dir: Option<std::path::PathBuf>,
    // Canonical Notes/ vault root, set only for notes-agent runs: the notes
    // tools refuse to work without it (SPEC notes chat slice).
    pub notes_root: Option<std::path::PathBuf>,
    // Shared app state handle: search opens its own DB connection through it,
    // fs/task tools borrow the UI connection from state.db.
    pub state: Option<std::sync::Arc<crate::state::AppState>>,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    #[allow(dead_code)] // taxonomy consumed by cfg(test) invariants, not by the loop
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

// Lenient coercion of weak-model scalar encodings, applied BEFORE strict
// validation: only properties the schema declares are touched, and only
// these conversions — clean numeric text to integer/number, "true"/"false"
// to boolean, numbers to strings, integral floats to integers. Arrays and
// objects are left to validation, missing fields are never invented.
pub fn coerce_args(schema: &Value, args: &Value) -> Value {
    let (Value::Object(obj), Some(props)) = (args, schema.get("properties").and_then(Value::as_object))
    else {
        return args.clone();
    };
    let mut out = Map::new();
    let mut changed = false;
    for (key, val) in obj {
        let target = props.get(key).and_then(|prop| prop.get("type")).and_then(Value::as_str);
        let coerced = match target {
            Some("number") => coerce_to_number(val, false),
            Some("integer") => coerce_to_number(val, true),
            Some("boolean") => coerce_to_bool(val),
            Some("string") => coerce_to_string(val),
            _ => val.clone(),
        };
        changed |= coerced != *val;
        out.insert(key.clone(), coerced);
    }
    if changed {
        Value::Object(out)
    } else {
        args.clone()
    }
}

fn coerce_to_number(val: &Value, integer_only: bool) -> Value {
    if let Some(text) = val.as_str() {
        let text = text.trim();
        if let Ok(int) = text.parse::<i64>() {
            return json!(int);
        }
        if let Ok(float) = text.parse::<f64>() {
            if float.is_finite() {
                if let Some(int) = integral_to_i64(float).filter(|_| integer_only) {
                    return json!(int);
                }
                if let Some(num) = serde_json::Number::from_f64(float) {
                    return Value::Number(num);
                }
            }
        }
    }
    if integer_only && !val.is_i64() {
        if let Some(float) = val.as_f64().and_then(integral_to_i64) {
            return json!(float);
        }
    }
    val.clone()
}

// f64 → i64 without the unstable TryFrom: exact enough for argument coercion.
fn integral_to_i64(float: f64) -> Option<i64> {
    if float.fract() == 0.0 && float >= i64::MIN as f64 && float <= i64::MAX as f64 {
        Some(float as i64)
    } else {
        None
    }
}

fn coerce_to_bool(val: &Value) -> Value {
    if let Some(text) = val.as_str() {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" => return json!(true),
            "false" => return json!(false),
            _ => {}
        }
    }
    val.clone()
}

fn coerce_to_string(val: &Value) -> Value {
    match val {
        Value::Number(num) => json!(num.to_string()),
        _ => val.clone(),
    }
}

// Minimal JSON-schema validator over the subset the catalog declares:
// object, required, types (string/integer/number/boolean/array/object),
// enum, additionalProperties. Unknown properties are rejected by default
// (additionalProperties:false semantics) unless the schema opts in with
// true or validates them against a schema-valued additionalProperties.
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
    match schema.get("additionalProperties") {
        // Schema-valued additionalProperties (e.g. a header map of strings):
        // extra properties are allowed but must validate against the schema.
        Some(extra) if extra.is_object() => {
            for (key, val) in obj {
                if props.is_some_and(|p| p.contains_key(key)) {
                    continue;
                }
                validate_value(extra, val, &format!("{path}{key}"))?;
            }
        }
        Some(Value::Bool(true)) => {}
        _ => {
            for key in obj.keys() {
                let known = props.map(|p| p.contains_key(key)).unwrap_or(false);
                if !known {
                    return Err(format!("unknown property \"{key}\""));
                }
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
        Some("integer") if !val.is_i64() && !val.is_u64() => {
            Err(format!("{path}: expected integer"))
        }
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
    fn validates_extra_properties_against_schema_valued_additional_properties() {
        let s = json!({
            "type": "object",
            "properties": { "headers": { "type": "object", "additionalProperties": { "type": "string" } } }
        });
        assert!(validate_args(&s, &json!({ "headers": { "X-Token": "abc", "Accept": "*/*" } })).is_ok());
        assert!(validate_args(&s, &json!({ "headers": { "n": 5 } }))
            .unwrap_err()
            .contains("string"));
        // declared properties still win over the additionalProperties schema
        assert!(validate_args(&s, &json!({ "headers": "not-an-object" }))
            .unwrap_err()
            .contains("object"));
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

    #[test]
    fn coerces_weak_scalar_encodings_per_schema() {
        let s = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "ratio": { "type": "number" },
                "flag": { "type": "boolean" },
                "label": { "type": "string" }
            },
            "additionalProperties": false
        });
        let args = json!({ "count": "5", "ratio": "2.5", "flag": "true", "label": 42 });
        assert_eq!(
            coerce_args(&s, &args),
            json!({ "count": 5, "ratio": 2.5, "flag": true, "label": "42" })
        );
        assert!(validate_args(&s, &coerce_args(&s, &args)).is_ok());
    }

    #[test]
    fn coerces_integral_floats_to_integer_only_when_declared() {
        let s = json!({ "type": "object", "properties": { "n": { "type": "integer" } } });
        assert_eq!(coerce_args(&s, &json!({ "n": 5.0 })), json!({ "n": 5 }));
        assert_eq!(coerce_args(&s, &json!({ "n": 5.5 })), json!({ "n": 5.5 }));
        // integer→number needs no conversion: serde_json integers validate as numbers.
        let s = json!({ "type": "object", "properties": { "n": { "type": "number" } } });
        assert_eq!(coerce_args(&s, &json!({ "n": 5 })), json!({ "n": 5 }));
        assert!(validate_args(&s, &coerce_args(&s, &json!({ "n": 5 }))).is_ok());
    }

    #[test]
    fn coercion_leaves_unknown_nested_and_invalid_values_untouched() {
        let s = json!({
            "type": "object",
            "properties": { "count": { "type": "integer" }, "nested": { "type": "object" } },
            "additionalProperties": true
        });
        let args = json!({ "count": "x", "nested": { "inner": "5" }, "extra": "7" });
        assert_eq!(coerce_args(&s, &args), args);
        // invalid numeric text survives coercion and is rejected by validation
        assert!(validate_args(&s, &coerce_args(&s, &json!({ "count": "x" })))
            .unwrap_err()
            .contains("integer"));
        // missing fields are never invented
        assert_eq!(coerce_args(&s, &json!({})), json!({}));
    }

    #[test]
    fn rejects_non_integer_for_integer_property() {
        let s = json!({ "type": "object", "properties": { "n": { "type": "integer" } } });
        assert!(validate_args(&s, &json!({ "n": 5 })).is_ok());
        assert!(validate_args(&s, &json!({ "n": "x" })).unwrap_err().contains("integer"));
    }
}
