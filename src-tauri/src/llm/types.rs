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

// One OpenAI-compatible multimodal content part. Internally tagged so the
// wire shape is exactly {type:"text",text} / {type:"image_url",image_url:{url}}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlPart },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrlPart {
    pub url: String,
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::ImageUrl {
            image_url: ImageUrlPart { url: url.into() },
        }
    }
}

// Message content: a plain string (the default; keeps the exact legacy wire
// shape) or an array of multimodal content parts (text + image_url).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Option<MessageContent>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    // Thinking-mode chain of thought of an assistant turn. Providers that run
    // in thinking mode reject a follow-up request unless the previous
    // assistant turn sends `reasoning_content` back, so the loop must carry it
    // through the transcript (serialized for assistant turns only).
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    fn text(role: Role, content: impl Into<String>) -> Self {
        ChatMessage {
            role,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
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
            reasoning_content: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::Tool,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            reasoning_content: None,
        }
    }

    // Multimodal user turn: content serializes as a parts array.
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        ChatMessage {
            role: Role::User,
            content: Some(MessageContent::Parts(parts)),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }

    // Attaches the thinking trace of an assistant turn (ignored for other
    // roles: the serializer only emits it for assistant messages).
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning_content = reasoning;
        self
    }

    // Text of a plain string content; None for multimodal parts or no content.
    pub fn text_content(&self) -> Option<&str> {
        match &self.content {
            Some(MessageContent::Text(text)) => Some(text),
            _ => None,
        }
    }
}

// Envelope of the synthetic tool result the sanitizer appends for a
// tool_call whose real result never made it into the transcript.
pub const MISSING_TOOL_RESULT_ENVELOPE: &str =
    r#"{"ok":false,"error":{"code":"cancelled","message":"tool result missing"}}"#;

// Repairs the model-facing view of a transcript so the API never sees an
// assistant turn with `tool_calls` that lacks a `tool` message for one of its
// tool_call_ids (HTTP 400 "An assistant message with 'tool_calls' must be
// followed by tool messages responding to each 'tool_call_id'"). Truncation,
// regeneration, branching, compaction or a run killed mid-tool can leave such
// holes in the stored rows; the stored transcript itself is never rewritten.
// Rules:
//   * every tool_call_id of an assistant turn gets a tool message before the
//     next assistant turn (or the end): missing ones are closed in place with
//     a synthetic `cancelled` envelope, the assistant turn itself is kept so
//     the context survives;
//   * a tool message whose tool_call_id matches no open tool_call (orphan,
//     duplicate, or left over from a truncation) is dropped;
//   * everything else keeps its order and content untouched.
pub fn sanitize_tool_history(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    fn close_pending(out: &mut Vec<ChatMessage>, pending: &mut Vec<String>) {
        for id in pending.drain(..) {
            out.push(ChatMessage::tool_result(id, MISSING_TOOL_RESULT_ENVELOPE));
        }
    }
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    // tool_call ids of the assistant turn whose results are still expected.
    let mut pending: Vec<String> = Vec::new();
    for msg in messages {
        if msg.role == Role::Tool {
            // A tool result only belongs to the currently open tool_calls
            // group; anything else (no open group, unknown or duplicate id)
            // is an orphan and is dropped.
            if let Some(pos) = msg
                .tool_call_id
                .as_ref()
                .and_then(|id| pending.iter().position(|p| p == id))
            {
                pending.remove(pos);
                out.push(msg.clone());
            }
            continue;
        }
        // Any other message closes the open group: ids still without a result
        // get their synthetic envelope before this message is appended.
        close_pending(&mut out, &mut pending);
        if msg.role == Role::Assistant && !msg.tool_calls.is_empty() {
            pending = msg.tool_calls.iter().map(|tc| tc.id.clone()).collect();
        }
        out.push(msg.clone());
    }
    close_pending(&mut out, &mut pending);
    out
}

// Hand-written to guarantee the exact OpenAI wire shape (snake_case keys,
// `tool_calls` wrapped as {id, type, function{name, arguments-as-string}}).
impl Serialize for ChatMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        // Thinking traces belong to assistant turns only: sending the key on
        // any other role is rejected by thinking-mode providers.
        let reasoning = match self.role {
            Role::Assistant => self
                .reasoning_content
                .as_deref()
                .filter(|r| !r.trim().is_empty()),
            _ => None,
        };
        let mut len = 2; // role, content
        if reasoning.is_some() {
            len += 1;
        }
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
        if let Some(reasoning) = reasoning {
            map.serialize_entry("reasoning_content", reasoning)?;
        }
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
    // Accumulated thinking trace of the round (providers report it as
    // `reasoning_content`, some stream it as `reasoning`).
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatEvent {
    Delta(String),
    ReasoningDelta(String),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_only_message_keeps_the_plain_string_wire_shape() {
        assert_eq!(
            serde_json::to_value(ChatMessage::user("hi")).unwrap(),
            json!({ "role": "user", "content": "hi" })
        );
        assert_eq!(
            serde_json::to_value(ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{}".into(),
            )]))
            .unwrap(),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "f", "arguments": "{}" }
                }]
            })
        );
    }

    #[test]
    fn multimodal_message_serializes_the_parts_array() {
        let msg = ChatMessage::user_parts(vec![
            ContentPart::text("что на картинке?"),
            ContentPart::image_url("data:image/png;base64,AAA="),
        ]);
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "что на картинке?" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA=" } }
                ]
            })
        );
    }

    #[test]
    fn text_content_reads_plain_string_content_only() {
        assert_eq!(ChatMessage::user("hi").text_content(), Some("hi"));
        let parts = ChatMessage::user_parts(vec![ContentPart::text("x")]);
        assert_eq!(parts.text_content(), None);
        assert_eq!(ChatMessage::assistant_tool_calls(vec![]).text_content(), None);
    }

    #[test]
    fn reasoning_content_is_sent_for_assistant_turns_only() {
        let asst = ChatMessage::assistant("answer").with_reasoning(Some("think".into()));
        assert_eq!(
            serde_json::to_value(&asst).unwrap(),
            json!({ "role": "assistant", "content": "answer", "reasoning_content": "think" })
        );
        // A thinking trace attached to a tool-call turn must travel back too.
        let calls = ChatMessage::assistant_tool_calls(vec![ToolCall::new(
            "c1".into(),
            "f".into(),
            "{}".into(),
        )])
        .with_reasoning(Some("plan".into()));
        let wire = serde_json::to_value(&calls).unwrap();
        assert_eq!(wire["reasoning_content"], "plan");
        // Other roles never carry the key, and an empty trace is dropped.
        assert_eq!(
            serde_json::to_value(ChatMessage::user("hi").with_reasoning(Some("x".into()))).unwrap(),
            json!({ "role": "user", "content": "hi" })
        );
        assert_eq!(
            serde_json::to_value(ChatMessage::assistant("a").with_reasoning(Some("  ".into())))
                .unwrap(),
            json!({ "role": "assistant", "content": "a" })
        );
        assert!(
            serde_json::to_value(ChatMessage::assistant("a")).unwrap()
                .get("reasoning_content")
                .is_none()
        );
    }

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id.into(), "f".into(), "{}".into())
    }

    fn asst_calls(ids: &[&str]) -> ChatMessage {
        ChatMessage::assistant_tool_calls(ids.iter().copied().map(call).collect())
    }

    // Wire-level check: every assistant tool_calls turn is followed by a tool
    // message per tool_call_id before the next assistant turn (or the end).
    fn tool_groups_are_closed(messages: &[ChatMessage]) -> bool {
        let mut pending: Vec<String> = Vec::new();
        for m in messages {
            if m.role == Role::Tool {
                match m.tool_call_id.as_ref().and_then(|id| pending.iter().position(|p| p == id)) {
                    Some(pos) => { pending.remove(pos); }
                    None => return false,
                }
                continue;
            }
            if !pending.is_empty() {
                return false;
            }
            if m.role == Role::Assistant {
                pending = m.tool_calls.iter().map(|tc| tc.id.clone()).collect();
            }
        }
        pending.is_empty()
    }

    #[test]
    fn sanitize_adds_synthetic_tool_result_for_missing_ids() {
        // assistant with 2 tool_calls, only 1 tool result → the missing one
        // is closed in place with the cancelled envelope.
        let history = vec![
            ChatMessage::user("go"),
            asst_calls(&["c1", "c2"]),
            ChatMessage::tool_result("c1", "{\"ok\":true}"),
            ChatMessage::assistant("done"),
        ];
        let out = sanitize_tool_history(&history);
        assert_eq!(out.len(), 5);
        assert_eq!(out[1].role, Role::Assistant);
        assert_eq!(out[1].tool_calls.len(), 2);
        assert_eq!(out[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(out[2].text_content(), Some("{\"ok\":true}"));
        assert_eq!(out[3].role, Role::Tool);
        assert_eq!(out[3].tool_call_id.as_deref(), Some("c2"));
        let envelope: serde_json::Value =
            serde_json::from_str(out[3].text_content().unwrap()).unwrap();
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["error"]["code"], "cancelled");
        assert_eq!(envelope["error"]["message"], "tool result missing");
        assert_eq!(out[4].text_content(), Some("done"));
        assert!(tool_groups_are_closed(&out));
    }

    #[test]
    fn sanitize_closes_groups_at_the_end_of_the_history() {
        let out = sanitize_tool_history(&[asst_calls(&["c1"])]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].tool_call_id.as_deref(), Some("c1"));
        assert!(tool_groups_are_closed(&out));
    }

    #[test]
    fn sanitize_drops_orphan_and_duplicate_tool_messages() {
        let history = vec![
            ChatMessage::tool_result("ghost", "orphan before any tool_calls"),
            ChatMessage::user("go"),
            asst_calls(&["c1"]),
            ChatMessage::tool_result("c1", "first"),
            ChatMessage::tool_result("c1", "duplicate"),
            ChatMessage::tool_result("other", "no such open id"),
            ChatMessage::assistant("done"),
        ];
        let out = sanitize_tool_history(&history);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[1].role, Role::Assistant);
        assert_eq!(out[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(out[2].text_content(), Some("first"));
        assert_eq!(out[3].text_content(), Some("done"));
        assert!(tool_groups_are_closed(&out));
    }

    #[test]
    fn sanitize_keeps_valid_transcripts_untouched() {
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("go"),
            asst_calls(&["c1", "c2"]),
            ChatMessage::tool_result("c1", "r1"),
            ChatMessage::tool_result("c2", "r2"),
            ChatMessage::assistant("done"),
        ];
        let out = sanitize_tool_history(&history);
        assert_eq!(
            serde_json::to_string(&out).unwrap(),
            serde_json::to_string(&history).unwrap()
        );
    }

    #[test]
    fn sanitized_history_gives_the_api_body_no_orphan_tool_calls() {
        // The exact 400 shape: an assistant tool_calls turn with a missing
        // tool result. After sanitizing, the serialized request body must
        // answer every tool_call_id with a following tool message.
        let broken = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("go"),
            asst_calls(&["c1", "c2"]),
            ChatMessage::tool_result("c1", "r1"),
        ];
        assert!(!tool_groups_are_closed(&broken));
        let params = crate::llm::ChatParams {
            model: "m".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            requires_reasoning_passback: false,
        };
        let body = crate::llm::build_body(&sanitize_tool_history(&broken), None, &params, false);
        let msgs = body["messages"].as_array().unwrap();
        for (i, m) in msgs.iter().enumerate() {
            let ids: Vec<&str> = m["tool_calls"]
                .as_array()
                .map(|arr| arr.iter().map(|tc| tc["id"].as_str().unwrap()).collect())
                .unwrap_or_default();
            for id in ids {
                let answered = msgs[i + 1..]
                    .iter()
                    .take_while(|next| next["role"] != "assistant")
                    .any(|next| {
                        next["role"] == "tool" && next["tool_call_id"].as_str() == Some(id)
                    });
                assert!(answered, "tool_call {id} has no tool message in the body");
            }
        }
    }
}
