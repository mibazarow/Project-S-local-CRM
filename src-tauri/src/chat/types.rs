// DTO shapes crossing the IPC boundary for chat transcripts (SPEC 3.3, 4.6).
use crate::attachments::AttachmentRef;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: i64,
    pub chat_id: i64,
    pub role: String,
    pub content: Option<String>,
    // Thinking trace of an assistant turn (SPEC reasoning slice): replayed to
    // the model and shown collapsed in the UI.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<Value>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub created_at: String,
    pub position: i64,
    // Saved attachment copies linked to this message (SPEC 6.1): user turns
    // carry files the user attached, assistant turns carry files the agent
    // sent with send_file. The raw content stays the plain text; the UI renders
    // the attachments as chips/thumbnails next to it.
    pub attachments: Vec<AttachmentRef>,
    // Pinned messages (SPEC chat UX slice) show in the compact bar above the
    // transcript; the flag lives on the message row itself.
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatBranchDto {
    pub chat_id: i64,
    pub session_id: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSearchHitDto {
    pub message_id: i64,
    pub role: String,
    pub snippet: String,
    pub created_at: String,
}
