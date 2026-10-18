// Vision tool (SPEC 6.1 / Q-17): analyze an image stored in the Notes vault
// through the configured OpenAI-compatible provider. It reuses the same vision
// plumbing the chat attachments use (llm::image_data_url + image content parts)
// rather than forking a second image path. Bound to the notes agent: the image
// is resolved strictly inside the Notes/ vault handed to the run, so a project
// or harness run cannot reach the vault through this tool. When the configured
// model has no vision support the tool fails with a clear error instead of
// silently dropping the image.
use super::project_tools::require_wd;
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::config::{read_config, ConfigKind, LlmConfig};
use crate::error::{AppError, Result};
use crate::llm::{self, ChatMessage, ChatParams, ContentPart, LlmClient};
use crate::fs_ops;
use serde_json::{json, Value};

pub struct VisionTool;

// Maps a vault path error to the envelope, mirroring the notes tools: a
// missing entry is not_found, path escapes and symlink refusals are
// permission_denied, everything else is an execution fault.
fn path_fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else if msg.contains("not Notes-relative")
        || msg.contains("not project-relative")
        || msg.contains("escapes the Notes")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else {
        ToolErrorCode::ExecutionError
    };
    ToolResult::err(code, msg)
}

impl Tool for VisionTool {
    fn name(&self) -> &str {
        "vision"
    }
    fn description(&self) -> &str {
        "Analyze an image stored in the Notes vault with the configured vision model. relPath is a vault-relative path to an image (png/jpg/jpeg/webp/gif/...); prompt says what to look for (default: describe the image). Use it to read text off a screenshot, describe a photo, or pull details out of an image note. Requires a vision-capable model in the LLM settings; if none is configured the tool returns a clear error rather than guessing."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relPath": { "type": "string" },
                "prompt": { "type": "string" }
            },
            "required": ["relPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn max_result_len(&self) -> usize {
        12_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let root = ctx
            .notes_root
            .clone()
            .ok_or_else(|| AppError::Invalid("no Notes vault bound to this agent run".into()))?;
        let wd = require_wd(ctx)?;
        let rel = args["relPath"].as_str().unwrap_or_default();
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .unwrap_or("Describe this image in detail.");

        let abs = match fs_ops::contain_existing(&root, rel, "Notes") {
            Ok(path) => path,
            Err(e) => return Ok(path_fail(e)),
        };

        let cfg: LlmConfig = serde_json::from_value(read_config(&wd, ConfigKind::Llm)?)
            .map_err(|e| AppError::Json(format!("llm.json: {e}")))?;
        if !cfg.vision_enabled() {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                "vision is not available: the configured model has no vision support. Pick a vision-capable model (or set the vision flag) in Settings, then retry.",
            ));
        }
        if cfg.base_url.trim().is_empty() {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                "LLM baseUrl is empty; configure the model endpoint in Settings",
            ));
        }

        let data_url = match llm::image_data_url(&abs) {
            Ok(url) => url,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };

        let client = LlmClient::new(&cfg.base_url, &cfg.api_key);
        let params = ChatParams::from(&cfg);
        let message = ChatMessage::user_parts(vec![
            ContentPart::text(prompt),
            ContentPart::image_url(data_url),
        ]);
        match client.chat(&[message], None, &params) {
            Ok(outcome) => Ok(ToolResult::ok(json!({
                "relPath": rel,
                "answer": outcome.content.unwrap_or_default(),
            }))
            .capped(self.max_result_len())),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::write_config;
    use crate::db;
    use crate::mcp::mock_server::{respond, Handler, MockServer};
    use crate::notes;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::state::AppState;
    use serde_json::json;
    use std::sync::Arc;

    // A working-dir with a Notes vault and an llm.json pointed at `base_url`.
    fn setup(tag: &str, base_url: &str, vision: bool) -> (Arc<AppState>, ToolCtx) {
        let wd = temp_test_dir(tag);
        let vault = notes::ensure(&wd).unwrap().canonicalize().unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        write_config(
            &wd,
            ConfigKind::Llm,
            json!({ "baseUrl": base_url, "model": "vision-model", "apiKey": "k", "vision": vision }),
        )
        .unwrap();
        let ctx = ToolCtx {
            notes_root: Some(vault),
            state: Some(Arc::clone(&state)),
            working_dir: Some(wd),
            ..Default::default()
        };
        (state, ctx)
    }

    fn put_image(vault: &std::path::Path, rel: &str) {
        let path = vault.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, crate::llm::TINY_PNG).unwrap();
    }

    // The provider is reached through a mock chat/completions endpoint; the
    // image travels as an image_url content part and the model's text comes
    // back as the tool answer.
    #[test]
    fn vision_routes_image_to_the_provider_and_returns_the_answer() {
        let handler: Handler = Arc::new(|stream: &mut std::net::TcpStream, req| {
            assert_eq!(req.path, "/chat/completions");
            let body: Value = serde_json::from_str(&req.body).unwrap();
            // the multimodal turn carries the prompt text plus an image part
            let parts = body["messages"][0]["content"].as_array().unwrap();
            assert_eq!(parts[0]["type"], "text");
            assert_eq!(parts[0]["text"], "what is on this image?");
            assert_eq!(parts[1]["type"], "image_url");
            assert!(parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
            respond(
                stream,
                200,
                "application/json",
                r#"{"choices":[{"message":{"content":"a red dot on a dark field"},"finish_reason":"stop"}]}"#,
                &[],
            )
        });
        let server = MockServer::start(handler);
        let (_state, ctx) = setup("vision-route", &server.url_base, true);
        let vault = ctx.notes_root.clone().unwrap();
        put_image(&vault, "скриншот.png");

        let r = VisionTool
            .execute(&json!({ "relPath": "скриншот.png", "prompt": "what is on this image?" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["relPath"], "скриншот.png");
        assert_eq!(data["answer"], "a red dot on a dark field");
    }

    // No vision-capable model configured: a clear error, no provider call.
    #[test]
    fn vision_reports_a_clear_error_when_the_model_has_no_vision() {
        let handler: Handler = Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "application/json", "{\"choices\":[]}", &[])
        });
        let server = MockServer::start(handler);
        let (_state, ctx) = setup("vision-missing", &server.url_base, false);
        let vault = ctx.notes_root.clone().unwrap();
        put_image(&vault, "картинка.png");

        let r = VisionTool
            .execute(&json!({ "relPath": "картинка.png" }), &ctx)
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert!(r.error.as_ref().unwrap().message.contains("vision"));
        // the provider was never contacted
        assert!(server.request_log().is_empty());
    }

    // The image path is resolved strictly inside the vault: escapes and missing
    // files never reach the provider.
    #[test]
    fn vision_rejects_paths_outside_the_vault_and_missing_images() {
        let handler: Handler = Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "application/json", "{\"choices\":[]}", &[])
        });
        let server = MockServer::start(handler);
        let (_state, ctx) = setup("vision-paths", &server.url_base, true);

        for bad in ["../x.png", "/etc/passwd"] {
            let r = VisionTool.execute(&json!({ "relPath": bad }), &ctx).unwrap();
            assert_eq!(
                r.error.as_ref().unwrap().code,
                "permission_denied",
                "accepted {bad:?}"
            );
        }
        let r = VisionTool
            .execute(&json!({ "relPath": "нет-такой.png" }), &ctx)
            .unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "not_found");
        assert!(server.request_log().is_empty());
    }

    // A non-image file in the vault is refused before any provider call.
    #[test]
    fn vision_rejects_non_image_files() {
        let handler: Handler = Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "application/json", "{\"choices\":[]}", &[])
        });
        let server = MockServer::start(handler);
        let (_state, ctx) = setup("vision-nonimage", &server.url_base, true);
        let vault = ctx.notes_root.clone().unwrap();
        std::fs::write(vault.join("нота.md"), "текст").unwrap();

        let r = VisionTool.execute(&json!({ "relPath": "нота.md" }), &ctx).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        assert!(server.request_log().is_empty());
    }

    #[test]
    fn vision_is_readonly_skip_and_schema_is_closed() {
        assert_eq!(VisionTool.class(), ToolClass::ReadOnly);
        assert_eq!(VisionTool.policy(), PermissionPolicy::Skip);
        assert!(VisionTool.danger_reason(&json!({ "relPath": "a.png" }), &ToolCtx::default()).is_none());
        let schema = VisionTool.parameters();
        assert!(crate::agent::tool::validate_args(&schema, &json!({ "relPath": "a.png" })).is_ok());
        assert!(crate::agent::tool::validate_args(&schema, &json!({ "relPath": "a.png", "prompt": "x" })).is_ok());
        assert!(crate::agent::tool::validate_args(&schema, &json!({ "prompt": "x" })).is_err());
        assert!(crate::agent::tool::validate_args(&schema, &json!({ "relPath": "a.png", "extra": 1 })).is_err());
    }
}
