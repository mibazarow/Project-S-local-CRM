// Model-facing and UI-facing transcript reads (SPEC 3.3, 4.6, 6.1): the
// history rebuild with attachment expansion and tool-call sanitization, plus
// the DTO listings for the chat UI.
use crate::attachments;
use crate::error::Result;
use crate::llm::{sanitize_tool_history, ChatMessage, ContentPart, MessageContent};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use super::store::{message_dto, project_chat_id, role_from_str, tool_calls_from_json};
use super::types::MessageDto;

// Rebuilds the model-facing transcript (system prompt excluded: the loop
// regenerates it per run). User turns that carry attachments are expanded
// through attachments::build_user_turn: documents contribute their extracted
// text, and images become image_url content parts when `vision_supported`
// (otherwise the turn carries an honest note and the image is not sent). The
// stored rows keep the plain user text; only this model-facing view is expanded.
pub fn load_history(
    conn: &Connection,
    chat_id: i64,
    vision_supported: bool,
) -> Result<Vec<ChatMessage>> {
    let base = chat_project_root(conn, chat_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, role, content, reasoning, tool_calls, tool_call_id, name
         FROM messages WHERE chat_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![chat_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, role, content, reasoning, tool_calls, tool_call_id, name) = row?;
        let content = if role == "user" {
            expand_user_turn(conn, id, content, base.as_deref(), vision_supported)?
        } else {
            content.map(MessageContent::Text)
        };
        out.push(ChatMessage {
            role: role_from_str(&role)?,
            content,
            tool_calls: match tool_calls {
                Some(t) => tool_calls_from_json(&t)?,
                None => Vec::new(),
            },
            tool_call_id,
            name,
            // Thinking traces of stored assistant turns travel back to the
            // model: thinking-mode providers reject a follow-up whose previous
            // assistant turn dropped `reasoning_content`.
            reasoning_content: reasoning,
        });
    }
    // The model-facing view is repaired on load: truncation, regeneration,
    // branching, compaction or a run killed mid-tool can leave an assistant
    // turn with tool_calls whose tool results are gone (HTTP 400 at send
    // time). The stored rows stay append-only and untouched; every run path
    // (project/harness/notes/schedule) builds its context through here.
    Ok(sanitize_tool_history(&out))
}

// Canonical project root for the chat's project; None for harness chats,
// whose attachment savedPaths are absolute anyway.
fn chat_project_root(conn: &Connection, chat_id: i64) -> Result<Option<PathBuf>> {
    Ok(conn
        .query_row(
            "SELECT p.path FROM chats c JOIN projects p ON p.id = c.project_id WHERE c.id = ?1",
            params![chat_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

// Rebuilds a user turn with its attachment blocks; messages without stored
// attachments keep their content untouched. When vision is supported and an
// image is present, the turn becomes a multimodal parts array (text part plus
// one image_url part per image); otherwise it stays a plain string with the
// document text injected and an honest note for any image.
fn expand_user_turn(
    conn: &Connection,
    message_id: i64,
    content: Option<String>,
    base: Option<&Path>,
    vision_supported: bool,
) -> Result<Option<MessageContent>> {
    let refs = attachments::load_message_attachments(conn, message_id)?;
    if refs.is_empty() {
        return Ok(content.map(MessageContent::Text));
    }
    let dtos: Vec<attachments::AttachmentDto> = refs.iter().map(attachments::ref_to_dto).collect();
    let text = content.unwrap_or_default();
    let turn = attachments::build_user_content(
        &text,
        &dtos,
        base.unwrap_or(Path::new("")),
        vision_supported,
    );
    if turn.images.is_empty() {
        return Ok(Some(MessageContent::Text(turn.text)));
    }
    let mut parts = Vec::with_capacity(turn.images.len() + 1);
    if !turn.text.is_empty() {
        parts.push(ContentPart::text(turn.text));
    }
    for url in turn.images {
        parts.push(ContentPart::image_url(url));
    }
    Ok(Some(MessageContent::Parts(parts)))
}

// UI-facing history for a chat by its id (shared by project chats and harness
// sessions).
pub fn history_by_chat(conn: &Connection, chat_id: i64) -> Result<Vec<MessageDto>> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE chat_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map(params![chat_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ids.iter().map(|id| message_dto(conn, *id)).collect()
}

// UI-facing history for the project's chat (empty when no chat exists yet).
pub fn chat_history(conn: &Connection, project_id: i64) -> Result<Vec<MessageDto>> {
    let Some(chat_id) = project_chat_id(conn, project_id)? else {
        return Ok(Vec::new());
    };
    history_by_chat(conn, chat_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::AttachmentRef;
    use crate::chat::test_support::{call, setup, tool_groups_are_closed};
    use crate::chat::{append_chat_message, append_message, ensure_project_chat};
    use crate::llm::Role;
    use crate::paths;
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    fn history_round_trips_tool_calls_in_order() {
        let (wd, conn, pid) = setup("chat-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("hello"), &[], None, None).unwrap();
        let calls = vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        append_message(
            &conn,
            chat_id,
            Role::Tool,
            Some("{\"ok\":true}"),
            &[],
            Some("c1"),
            None,
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("done"), &[], None, None).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[1].tool_calls.len(), 1);
        assert_eq!(hist[1].tool_calls[0].id, "c1");
        assert_eq!(hist[1].tool_calls[0].name, "read_file");
        assert_eq!(hist[1].tool_calls[0].raw_arguments, "{\"rel_path\":\"a.md\"}");
        assert_eq!(hist[2].role, Role::Tool);
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].text_content(), Some("done"));

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 4);
        assert_eq!(dtos[0].role, "user");
        assert_eq!(dtos[1].tool_calls[0]["name"], "read_file");
        assert_eq!(dtos[2].tool_call_id.as_deref(), Some("c1"));
        assert!(dtos[0].position < dtos[1].position);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_closes_open_tool_calls_with_synthetic_results() {
        let (wd, conn, pid) = setup("chat-sanitize-load");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let calls = vec![
            call("c1", "read_file", json!({ "rel_path": "a.md" })),
            call("c2", "read_file", json!({ "rel_path": "b.md" })),
        ];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        // Only c1's result was persisted (a killed run lost c2's).
        append_message(&conn, chat_id, Role::Tool, Some("{\"ok\":true}"), &[], Some("c1"), None)
            .unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4, "the synthetic tool message closes the group");
        assert_eq!(hist[3].role, Role::Tool);
        assert_eq!(hist[3].tool_call_id.as_deref(), Some("c2"));
        let envelope: Value = serde_json::from_str(hist[3].text_content().unwrap()).unwrap();
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["error"]["code"], "cancelled");
        assert_eq!(envelope["error"]["message"], "tool result missing");
        assert!(tool_groups_are_closed(&hist));
        // The stored rows stay untouched: the repair is model-facing only.
        let stored: i64 = conn
            .query_row("SELECT count(*) FROM messages WHERE chat_id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 3);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn reasoning_round_trips_through_the_db_back_into_the_model_transcript() {
        let (wd, conn, pid) = setup("chat-reasoning");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let asst = ChatMessage::assistant("done").with_reasoning(Some("because".into()));
        append_chat_message(&conn, chat_id, &asst).unwrap();

        // The UI DTO carries the trace so the transcript can show it.
        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos[0].reasoning, None);
        assert_eq!(dtos[1].reasoning.as_deref(), Some("because"));

        // The model-facing transcript replays it as reasoning_content.
        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist[1].reasoning_content.as_deref(), Some("because"));
        let wire = serde_json::to_value(&hist).unwrap();
        assert_eq!(wire[1]["reasoning_content"], "because");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn send_persists_attachments_linked_to_user_message_and_history_returns_them() {
        let (wd, conn, pid) = setup("chat-attach-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("проверь счёт"), &[], None, None)
            .unwrap();
        let refs = vec![
            AttachmentRef {
                name: "счёт.docx".into(),
                saved_path: "Входящие/счёт.docx".into(),
                kind: "document".into(),
                is_image: false,
            },
            AttachmentRef {
                name: "pic.png".into(),
                saved_path: "Входящие/pic.png".into(),
                kind: "image".into(),
                is_image: true,
            },
        ];
        attachments::save_message_attachments(&conn, user.id, &refs).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("ок"), &[], None, None).unwrap();

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 2);
        assert_eq!(dtos[0].attachments.len(), 2);
        assert_eq!(dtos[0].attachments[0].name, "счёт.docx");
        assert_eq!(dtos[0].attachments[0].saved_path, "Входящие/счёт.docx");
        assert_eq!(dtos[0].attachments[0].kind, "document");
        assert!(dtos[0].attachments[1].is_image);
        assert!(dtos[1].attachments.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_expands_document_text_and_notes_images_when_vision_off() {
        let (wd, conn, pid) = setup("chat-attach-turn");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let turn = hist[0].text_content().unwrap();
        assert!(turn.starts_with("что тут?"));
        // Documents contribute their extracted text to the model turn...
        assert!(turn.contains("[Attachment: a.md]\nданные проекта"));
        // ...images are honestly noted and not sent (no base64 in the turn).
        assert!(turn.contains("[Image \"pic.png\" not sent: the model does not support vision]"));
        assert!(!turn.contains("base64"));

        // The stored row keeps the plain user text: the UI renders the
        // attachments as separate chips next to the bubble.
        let raw: String = conn
            .query_row("SELECT content FROM messages WHERE id = ?1", params![user.id], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "что тут?");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_sends_image_parts_when_vision_on() {
        let (wd, conn, pid) = setup("chat-attach-turn-vision");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, true).unwrap();
        // With vision on, the user turn is a parts array: text + image_url.
        let wire = serde_json::to_value(&hist[0]).unwrap();
        let parts = wire["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"]
            .as_str()
            .unwrap()
            .contains("[Attachment: a.md]\nданные проекта"));
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        // No honest "not sent" note when the image is actually sent.
        assert!(!serde_json::to_string(&hist[0]).unwrap().contains("not sent"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
