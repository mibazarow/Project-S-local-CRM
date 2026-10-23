use serde_json::{json, Value};

// Adds `"reasoning_content": ""` to every assistant message object that lacks
// the key, leaving turns that already carry a trace untouched.
pub(crate) fn ensure_reasoning_on_assistants(messages: Option<&mut Vec<Value>>) {
    if let Some(messages) = messages {
        for m in messages {
            if m.get("role").and_then(Value::as_str) == Some("assistant")
                && m.get("reasoning_content").is_none()
            {
                m["reasoning_content"] = json!("");
            }
        }
    }
}

// The thinking-mode pass-back rejection: HTTP 400 whose body names
// `reasoning_content` anywhere (case-insensitive; JSON wrapping, quoting and
// backticks around the field name are irrelevant to the substring match).
// Deliberately not keyed on provider or model names: any gateway enforcing the
// rule answers with this exact class of error.
pub(crate) fn is_reasoning_passback_error(status: u16, body: &str) -> bool {
    status == 400 && body.to_ascii_lowercase().contains("reasoning_content")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{build_body, ChatMessage, ChatParams, LlmClient, ToolCall, ToolSpec};
    use serde_json::{json, Value};

    #[test]
    fn build_body_fills_missing_reasoning_on_assistant_turns_for_deepseek() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("plain answer"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{}".into(),
            )]),
            ChatMessage::assistant("traced").with_reasoning(Some("real thought".into())),
            ChatMessage::tool_result("c1", "ok"),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let params = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let body = build_body(&messages, Some(&tools), &params, false);
        let msgs = body["messages"].as_array().unwrap();
        // Non-assistant roles are never given the key.
        assert!(msgs[0].get("reasoning_content").is_none());
        assert!(msgs[1].get("reasoning_content").is_none());
        assert!(msgs[5].get("reasoning_content").is_none());
        // Assistant turns without a trace (plain and tool-call) get an empty one.
        assert_eq!(msgs[2]["reasoning_content"], "");
        assert!(msgs[3].get("tool_calls").is_some());
        assert_eq!(msgs[3]["reasoning_content"], "");
        // An existing non-empty trace is preserved, not overwritten.
        assert_eq!(msgs[4]["reasoning_content"], "real thought");
    }

    #[test]
    fn build_body_adds_no_reasoning_field_for_other_providers() {
        let messages = vec![
            ChatMessage::assistant("plain"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{}".into(),
            )]),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let params = ChatParams {
            model: "gpt-4o-mini".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let body = build_body(&messages, Some(&tools), &params, false);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[0].get("reasoning_content").is_none());
        assert!(msgs[1].get("reasoning_content").is_none());
    }

    #[test]
    fn build_body_passback_is_noop_without_tools() {
        let messages = vec![ChatMessage::assistant("plain")];
        let params = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let body = build_body(&messages, None, &params, false);
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_mock_rejects_body_missing_reasoning_passback() {
        use crate::mcp::mock_server::{respond, CapturedRequest, Handler, MockServer};

        // DeepSeek-like gate: HTTP 400 unless every assistant turn carries
        // `reasoning_content`; 200 with a valid completion otherwise.
        let handler: Handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                        &[],
                    )
                } else {
                    respond(
                        stream,
                        200,
                        "application/json",
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
                        &[],
                    )
                }
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");

        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("earlier"),
            ChatMessage::user("again"),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];

        // Gate on (DeepSeek thinking): the empty trace is filled in proactively,
        // so the provider accepts the follow-up on the first attempt.
        let fixed = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let out = client.chat(&messages, Some(&tools), &fixed).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        assert_eq!(server.request_log().len(), 1);

        // The wire body actually carried the empty trace on the assistant turn.
        let sent: Value =
            serde_json::from_str(&server.request_log().last().unwrap().body).unwrap();
        assert_eq!(sent["messages"][1]["reasoning_content"], "");

        // Gate off: the proactive fill is skipped, the mock 400s, and the
        // provider-agnostic reasoning retry repairs the follow-up on the
        // second attempt.
        let legacy = ChatParams {
            requires_reasoning_passback: false,
            ..fixed.clone()
        };
        let out = client.chat(&messages, Some(&tools), &legacy).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 3);
        let first: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert!(first["messages"][1].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[2].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "");
    }

    #[test]
    fn reasoning_passback_error_detection_is_provider_agnostic() {
        // The exact production shape (backticks, request id, JSON wrapping).
        assert!(is_reasoning_passback_error(
            400,
            r#"{"error":{"message":"The `reasoning_content` in the thinking mode must be passed back to the API. (request_id: 94a2e983-5ada-4c47-9499-0ff7821cd23b)","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#
        ));
        assert!(is_reasoning_passback_error(400, "REASONING_CONTENT MUST BE PASSED BACK"));
        assert!(is_reasoning_passback_error(400, "  reasoning_content  "));
        // Other 400s and other statuses saying the word are not this error.
        assert!(!is_reasoning_passback_error(400, "model does not support image inputs"));
        assert!(!is_reasoning_passback_error(401, "reasoning_content"));
        assert!(!is_reasoning_passback_error(500, "reasoning_content"));
    }

    // A gateway under a non-DeepSeek name that always enforces the pass-back:
    // 400 while any assistant turn omits the field, 200 otherwise.
    fn passback_gate_handler() -> crate::mcp::mock_server::Handler {
        use crate::mcp::mock_server::{respond, CapturedRequest};
        std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"The `reasoning_content` in the thinking mode must be passed back to the API.","type":"invalid_request_error"}}"#,
                        &[],
                    )
                } else {
                    respond(
                        stream,
                        200,
                        "application/json",
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
                        &[],
                    )
                }
            },
        )
    }

    #[test]
    fn reasoning_400_retries_once_with_and_without_tools() {
        use crate::mcp::mock_server::MockServer;

        let server = MockServer::start(passback_gate_handler());
        let client = LlmClient::new(&server.url_base, "k");
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("earlier").with_reasoning(Some("real trace".into())),
            ChatMessage::assistant("plain"),
            ChatMessage::user("again"),
        ];
        // Heuristic gate off: neither the endpoint nor the model says deepseek.
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];

        // With tools: exactly one retry; the retry preserves the real trace
        // and fills the untraced assistant turn with an empty one.
        let out = client.chat(&messages, Some(&tools), &params).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 2);
        let first: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert!(first["messages"][2].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "real trace");
        assert_eq!(retry["messages"][2]["reasoning_content"], "");

        // Without tools: the same recovery (the proactive patch never applies
        // here, only the 400-triggered retry does).
        let out = client.chat(&messages, None, &params).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 4);
        let first: Value = serde_json::from_str(&sent[2].body).unwrap();
        assert!(first["messages"][2].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[3].body).unwrap();
        assert_eq!(retry["messages"][2]["reasoning_content"], "");
    }

    #[test]
    fn reasoning_400_retry_happens_exactly_once() {
        use crate::mcp::mock_server::{respond, CapturedRequest, MockServer};

        // Always-400 mock: the client must not loop, the error surfaces after
        // exactly one corrected retry.
        let handler: crate::mcp::mock_server::Handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, _req: &CapturedRequest| {
                respond(
                    stream,
                    400,
                    "application/json",
                    r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                    &[],
                )
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("earlier")];
        let err = client.chat(&messages, None, &params).unwrap_err();
        assert!(err.to_string().contains("400"), "expected HTTP 400, got {err}");
        assert_eq!(server.request_log().len(), 2);
    }

    #[test]
    fn reasoning_400_retries_once_for_streaming_requests() {
        use crate::mcp::mock_server::{write_sse_headers, MockServer};
        use std::io::Write;

        let handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &crate::mcp::mock_server::CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    crate::mcp::mock_server::respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                        &[],
                    )
                } else {
                    write_sse_headers(stream)?;
                    stream.write_all(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
                    )?;
                    stream.write_all(b"data: [DONE]\n\n")
                }
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("earlier")];
        let out = client
            .chat_stream(&messages, None, &params, |_| {})
            .unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 2);
        let retry: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "");
        assert_eq!(retry["stream"], json!(true));
    }
}
