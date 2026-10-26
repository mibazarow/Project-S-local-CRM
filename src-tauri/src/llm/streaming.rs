use crate::error::{AppError, Result};
use crate::llm::types::{ChatEvent, ChatOutcome, ToolCall, Usage};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::BufRead;
use std::time::Duration;

#[derive(Default)]
struct StreamAcc {
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    // index-keyed fragments: (id, name, arguments) accumulated across chunks.
    frags: BTreeMap<usize, (String, String, String)>,
}

impl StreamAcc {
    fn apply(&mut self, chunk: &Value, on_event: &mut dyn FnMut(ChatEvent)) {
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            if let Ok(usage) = serde_json::from_value(u.clone()) {
                self.usage = Some(usage);
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !c.is_empty() {
                self.content.push_str(c);
                on_event(ChatEvent::Delta(c.to_string()));
            }
        }
        // Thinking deltas: DeepSeek-style `reasoning_content`, some gateways
        // stream the same trace as `reasoning`. Both accumulate and stream;
        // only one key per delta is read so an aliasing provider cannot
        // double-count the same fragment.
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str);
        if let Some(r) = reasoning {
            if !r.is_empty() {
                self.reasoning.push_str(r);
                on_event(ChatEvent::ReasoningDelta(r.to_string()));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let frag = self.frags.entry(index).or_default();
                let id = tc.get("id").and_then(Value::as_str);
                if let Some(id) = id {
                    frag.0.push_str(id);
                }
                let func = tc.get("function");
                let name = func.and_then(|f| f.get("name")).and_then(Value::as_str);
                if let Some(name) = name {
                    frag.1.push_str(name);
                }
                let arguments = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                frag.2.push_str(arguments);
                on_event(ChatEvent::ToolCallDelta {
                    index,
                    id: id.map(String::from),
                    name: name.map(String::from),
                    arguments_delta: arguments.to_string(),
                });
            }
        }
    }

    fn finish(self, on_event: &mut dyn FnMut(ChatEvent)) -> ChatOutcome {
        let tool_calls: Vec<ToolCall> = self
            .frags
            .into_iter()
            .map(|(i, (id, name, arguments))| {
                let id = if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                };
                ToolCall::new(id, name, arguments)
            })
            .collect();
        if !tool_calls.is_empty() {
            on_event(ChatEvent::ToolCallsFinal(tool_calls.clone()));
        }
        on_event(ChatEvent::Usage(self.usage.clone()));
        on_event(ChatEvent::Done);
        ChatOutcome {
            content: (!self.content.is_empty()).then_some(self.content),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

pub(crate) fn run_sse(
    reader: impl std::io::Read,
    inactivity: Duration,
    on_event: &mut dyn FnMut(ChatEvent),
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<ChatOutcome> {
    let mut acc = StreamAcc::default();
    let mut buf = std::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            // The stream agent's per-read socket timeout surfaces here as a
            // TimedOut io error once `inactivity` passes with zero bytes. Map
            // it to a distinct, user-readable error; the reader (and its
            // socket) is dropped on return, closing the connection.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(AppError::LlmTimeout(format!(
                    "Провайдер не отвечает {} секунд. Проверьте баланс, доступность API и лимиты токенов.",
                    inactivity.as_secs()
                )));
            }
            Err(e) => return Err(e.into()),
        }
        if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
            return Ok(acc.finish(on_event));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(payload)?;
        acc.apply(&chunk, on_event);
    }
    Ok(acc.finish(on_event))
}

pub(crate) fn parse_non_stream(value: &Value) -> Result<ChatOutcome> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| AppError::Json("chat response has no choices".into()))?;
    let message = &choice["message"];
    let content = message.get("content").and_then(Value::as_str).map(String::from);
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .map(String::from);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let func = tc.get("function")?;
                    Some(ToolCall::new(
                        tc.get("id").and_then(Value::as_str)?.to_string(),
                        func.get("name").and_then(Value::as_str)?.to_string(),
                        func.get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ChatOutcome {
        content,
        reasoning,
        tool_calls,
        finish_reason: choice.get("finish_reason").and_then(Value::as_str).map(String::from),
        usage: value
            .get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::DEFAULT_STREAM_INACTIVITY_TIMEOUT;
    use serde_json::json;
    use std::io::Cursor;

    fn sse(lines: &[&str]) -> (Vec<ChatEvent>, ChatOutcome) {
        let text: String = lines.iter().map(|l| format!("{l}\n\n")).collect();
        let mut events = Vec::new();
        let outcome = run_sse(
            Cursor::new(text),
            DEFAULT_STREAM_INACTIVITY_TIMEOUT,
            &mut |e| events.push(e),
            None,
        )
        .unwrap();
        (events, outcome)
    }

    #[test]
    fn sse_stream_stops_at_the_cancel_flag_and_keeps_the_partial_round() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // The flag flips after the first chunk: the reader must stop there,
        // the remaining chunks are never applied, and the partial content is
        // returned (the loop maps it to the "cancelled" finish reason).
        let cancel = AtomicBool::new(false);
        let text = "[DONE]";
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"content":"Первая "}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"вторая"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            text,
        ];
        let stream: String = lines.iter().map(|l| format!("{l}\n\n")).collect();
        let mut events = Vec::new();
        let outcome = run_sse(
            Cursor::new(stream),
            DEFAULT_STREAM_INACTIVITY_TIMEOUT,
            &mut |e| {
                if matches!(e, ChatEvent::Delta(_)) && !cancel.load(Ordering::Relaxed) {
                    cancel.store(true, Ordering::Relaxed);
                }
                events.push(e);
            },
            Some(&cancel),
        )
        .unwrap();
        let deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Первая "]);
        assert_eq!(outcome.content.as_deref(), Some("Первая "));
    }

    #[test]
    fn sse_stream_accumulates_text_and_split_tool_call() {
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\": "}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}"#,
            "data: [DONE]",
        ]);

        assert_eq!(outcome.content.as_deref(), Some("Hello"));
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(outcome.usage.clone().unwrap().total_tokens, 12);
        assert_eq!(outcome.tool_calls.len(), 1);
        let tc = &outcome.tool_calls[0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "read_file");
        assert_eq!(tc.arguments, json!({ "path": "a.txt" }));
        assert!(tc.arguments_error.is_none());

        let deltas = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::Delta(_)))
            .count();
        assert_eq!(deltas, 2);
        assert!(events.iter().any(|e| matches!(e, ChatEvent::ToolCallDelta { index: 0, .. })));
        assert!(matches!(events.last(), Some(ChatEvent::Done)));
    }

    #[test]
    fn sse_stream_flushes_one_delta_per_content_chunk_in_order() {
        // Progressive display depends on the backend emitting one Delta per SSE
        // data event (no buffering to the full response): N content chunks must
        // yield N ordered, non-empty deltas whose concatenation is the answer.
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"При"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"вет"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"!"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        let streamed: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["При", "вет", "!"]);
        assert_eq!(outcome.content.as_deref(), Some("Привет!"));
    }

    #[test]
    fn sse_stream_one_chunk_delivers_the_whole_answer_in_a_single_delta() {
        // A provider that buffers the whole answer into one SSE chunk still
        // yields exactly one delta carrying the full text (the frontend then
        // reveals it with a typewriter fallback).
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"Весь ответ одним куском"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        let deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Весь ответ одним куском"]);
        assert_eq!(outcome.content.as_deref(), Some("Весь ответ одним куском"));
    }

    #[test]
    fn sse_stream_accumulates_reasoning_from_both_delta_keys() {
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"Thin"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"king"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"reasoning":" deeper"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(outcome.reasoning.as_deref(), Some("Thinking deeper"));
        assert_eq!(outcome.content.as_deref(), Some("Hi"));
        let streamed: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ReasoningDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["Thin", "king", " deeper"]);
    }

    #[test]
    fn sse_invalid_tool_arguments_keep_raw_and_mark_error() {
        let (_, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{oops"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        let tc = &outcome.tool_calls[0];
        assert_eq!(tc.raw_arguments, "{oops");
        assert!(tc.arguments_error.is_some());
        assert_eq!(tc.arguments, Value::Null);
    }

    #[test]
    fn non_stream_response_parses_to_same_outcome() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "reasoning_content": "because",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "f", "arguments": "{\"a\":1}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 }
        });
        let outcome = parse_non_stream(&value).unwrap();
        assert_eq!(outcome.content.as_deref(), Some("hi"));
        assert_eq!(outcome.reasoning.as_deref(), Some("because"));
        assert_eq!(outcome.tool_calls[0].arguments, json!({ "a": 1 }));
        assert_eq!(outcome.usage.unwrap().total_tokens, 3);
    }

    // A reader that yields a TimedOut io error on the first read stands in for
    // the stream agent's per-read socket timeout firing after the inactivity
    // window with zero bytes (SPEC 6.0.1, Q23).
    struct SilentReader {
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::io::Read for SilentReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no data"))
        }
    }

    impl Drop for SilentReader {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn run_sse_aborts_on_inactivity_with_distinct_error_and_closes_reader() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let err = run_sse(
            SilentReader {
                dropped: std::sync::Arc::clone(&dropped),
            },
            DEFAULT_STREAM_INACTIVITY_TIMEOUT,
            &mut |_| {},
            None,
        )
        .unwrap_err();
        // Distinct, user-readable error the frontend can offer «Повторить» for.
        assert!(matches!(err, AppError::LlmTimeout(_)), "{err}");
        assert_eq!(err.code(), "llm-timeout");
        let msg = err.to_string();
        assert!(msg.contains("180"), "message names the threshold: {msg}");
        assert!(msg.contains("секунд"), "message is Russian: {msg}");
        // The serialized payload carries the distinct code for the UI.
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "llm-timeout");
        // The reader (and its socket) is dropped on the early return: cleanup.
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "reader must be dropped when the watchdog aborts"
        );
    }

    // A provider that keeps sending SSE lines, however slowly, must NOT abort:
    // the watchdog is per-read inactivity, not a total deadline. The reader
    // sleeps between reads (total well past the tiny threshold passed in) yet
    // run_sse completes with the full answer.
    struct SlowReader {
        parts: Vec<Vec<u8>>,
        idx: usize,
        gap: Duration,
    }

    impl std::io::Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.idx >= self.parts.len() {
                return Ok(0);
            }
            std::thread::sleep(self.gap);
            let part = &self.parts[self.idx];
            let n = part.len().min(buf.len());
            buf[..n].copy_from_slice(&part[..n]);
            self.idx += 1;
            Ok(n)
        }
    }

    #[test]
    fn run_sse_does_not_abort_a_slow_but_live_stream() {
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"content":"При"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"вет"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"!"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        let parts: Vec<Vec<u8>> = lines.iter().map(|l| format!("{l}\n").into_bytes()).collect();
        // 5 reads x 6 ms = 30 ms of live streaming, past a 1 ms threshold.
        let reader = SlowReader {
            parts,
            idx: 0,
            gap: Duration::from_millis(6),
        };
        let mut streamed = Vec::new();
        let outcome = run_sse(reader, Duration::from_millis(1), &mut |e| {
            if let ChatEvent::Delta(t) = e {
                streamed.push(t);
            }
        }, None)
        .expect("slow-but-live stream must not abort");
        assert_eq!(streamed, ["При", "вет", "!"]);
        assert_eq!(outcome.content.as_deref(), Some("Привет!"));
    }
}
