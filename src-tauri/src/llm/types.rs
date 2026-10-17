use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

// Accumulated tool call. `arguments` is parsed from `raw_arguments`; a parse
// failure keeps the raw string and records the error so the agent loop can
// report it back to the model instead of crashing.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub arguments_error: Option<String>,
}

impl ToolCall {
    pub fn new(id: String, name: String, raw_arguments: String) -> Self {
        let (arguments, arguments_error) = if raw_arguments.trim().is_empty() {
            (Value::Object(Map::new()), None)
        } else {
            match serde_json::from_str(&raw_arguments) {
                Ok(v) => (v, None),
                Err(e) => (Value::Null, Some(e.to_string())),
            }
        };
        ToolCall {
            id,
            name,
            arguments,
            raw_arguments,
            arguments_error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

impl ChatMessage {
    fn text(role: Role, content: impl Into<String>) -> Self {
        ChatMessage {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        ChatMessage {
            role: Role::Assistant,
            content: None,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
        }
    }
}

// Hand-written to guarantee the exact OpenAI wire shape (snake_case keys,
// `tool_calls` wrapped as {id, type, function{name, arguments-as-string}}).
impl Serialize for ChatMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut len = 2; // role, content
        if !self.tool_calls.is_empty() {
            len += 1;
        }
        if self.tool_call_id.is_some() {
            len += 1;
        }
        if self.name.is_some() {
            len += 1;
        }
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("role", &self.role)?;
        map.serialize_entry("content", &self.content)?;
        if !self.tool_calls.is_empty() {
            let wire: Vec<Value> = self
                .tool_calls
                .iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.raw_arguments },
                    })
                })
                .collect();
            map.serialize_entry("tool_calls", &wire)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        if let Some(name) = &self.name {
            map.serialize_entry("name", name)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatOutcome {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatEvent {
    Delta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCallsFinal(Vec<ToolCall>),
    Usage(Option<Usage>),
    Done,
}
