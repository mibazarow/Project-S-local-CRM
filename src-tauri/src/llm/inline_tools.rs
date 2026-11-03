// Inline tool-call fallback parser.
//
// Some OpenAI-compatible gateways/models answer with the tool call written as
// text instead of the native `tool_calls` array — typically the Qwen/DeepSeek
// XML dialect:
//
//   <tool_call>
//   <function=webfetch>
//   <parameter=url>
//   https://example.ru
//   </parameter>
//   </tool_call>
//
// Native tool calls are always preferred; this parser only runs when the
// provider sent none, so a correct stream is never reinterpreted. It also
// strips the blocks from the visible text (content and reasoning), which is
// what leaked into the transcript before.
use crate::llm::types::ToolCall;
use serde_json::{Map, Value};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// Extract text-form tool calls and return the remaining prose.
pub(crate) fn extract(text: &str) -> (Vec<ToolCall>, String) {
    if !text.contains(OPEN) {
        // Nothing to clean: return the text verbatim so untouched whitespace
        // (indentation, a trailing space) survives.
        return (Vec::new(), text.to_string());
    }
    let mut calls = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find(OPEN) {
        cleaned.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let (body, tail) = match after.find(CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            // An unterminated block still counts: the model often drops the
            // closing tag when the turn is cut at the tool boundary.
            None => (after, ""),
        };
        if let Some(call) = parse_block(body, calls.len()) {
            calls.push(call);
        }
        rest = tail;
    }
    cleaned.push_str(rest);
    let cleaned = cleaned.trim().to_string();
    (calls, cleaned)
}

/// Only the leftover markup matters for display; keep the prose around it.
pub(crate) fn strip(text: &str) -> String {
    extract(text).1
}

fn parse_block(body: &str, index: usize) -> Option<ToolCall> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Some(call) = parse_json_body(body, index) {
        return Some(call);
    }
    let (name, rest) = parse_function_open(body)?;
    let arguments = parse_parameters(rest);
    Some(ToolCall::new(
        format!("call_inline_{index}"),
        name,
        Value::Object(arguments).to_string(),
    ))
}

// {"name": "webfetch", "arguments": {...}} / {"arguments": ..., "name": ...}
fn parse_json_body(body: &str, index: usize) -> Option<ToolCall> {
    let trimmed = body.trim_start_matches("```json").trim_start_matches("```").trim();
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let name = value.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    let arguments = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let arguments = match arguments {
        Value::String(raw) => {
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        other => other,
    };
    Some(ToolCall::new(
        format!("call_inline_{index}"),
        name.to_string(),
        arguments.to_string(),
    ))
}

// <function=webfetch> / <function name="webfetch"> / <function>webfetch</function>
fn parse_function_open(body: &str) -> Option<(String, &str)> {
    let open = body.find("<function")?;
    let after = &body[open + "<function".len()..];
    let (name, rest) = if let Some(rest) = after.strip_prefix('=') {
        let end = rest.find('>')?;
        (rest[..end].trim().to_string(), &rest[end + 1..])
    } else if let Some(rest) = after.strip_prefix(" name=") {
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &rest[quote.len_utf8()..];
        let end = rest.find(quote)?;
        let name = rest[..end].trim().to_string();
        let rest = &rest[end + quote.len_utf8()..];
        let end = rest.find('>')?;
        (name, &rest[end + 1..])
    } else {
        let end = after.find('>')?;
        let inner = after[..end].trim().to_string();
        if inner.is_empty() {
            let tail = &after[end + 1..];
            let close = tail.find("</function>").unwrap_or(tail.len());
            (tail[..close].trim().to_string(), &tail[close..])
        } else {
            (inner, &after[end + 1..])
        }
    };
    (!name.is_empty()).then_some((name, rest))
}

// <parameter=key>value</parameter> repeated; a missing close tag ends the
// value at the next parameter or at the end of the block.
fn parse_parameters(body: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let mut rest = body;
    while let Some(open) = rest.find("<parameter") {
        let after = &rest[open + "<parameter".len()..];
        let Some(rest_after) = after.strip_prefix('=') else {
            rest = after;
            continue;
        };
        let Some(end) = rest_after.find('>') else {
            break;
        };
        let key = rest_after[..end].trim().to_string();
        let tail = &rest_after[end + 1..];
        let (value, next) = match tail.find("</parameter>") {
            Some(close) => (&tail[..close], &tail[close + "</parameter>".len()..]),
            None => {
                let stop = tail.find("<parameter").unwrap_or(tail.len());
                (&tail[..stop], &tail[stop..])
            }
        };
        if !key.is_empty() {
            out.insert(key, Value::String(value.trim().to_string()));
        }
        rest = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact shape reported from the wild: no </function>, sometimes no
    // </parameter>, several blocks in one answer.
    const REPORTED: &str = "<tool_call>\n<function=webfetch>\n<parameter=url>\nhttps://plusvibeapi.ru/model/deepseek-v4.1-flash-1017\n\n</parameter>\n</tool_call><tool_call>\n<function=http_request>\n<parameter=method>\nGET\n\n<parameter=url>\nhttps://plusvibeapi.ru/model/x\n\n</tool_call>";

    #[test]
    fn parses_the_reported_dialect() {
        let (calls, cleaned) = extract(REPORTED);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "webfetch");
        assert_eq!(
            calls[0].arguments["url"],
            "https://plusvibeapi.ru/model/deepseek-v4.1-flash-1017"
        );
        assert_eq!(calls[1].name, "http_request");
        assert_eq!(calls[1].arguments["method"], "GET");
        assert_eq!(calls[1].arguments["url"], "https://plusvibeapi.ru/model/x");
        assert_eq!(cleaned, "");
    }

    #[test]
    fn keeps_the_prose_around_the_blocks() {
        let text = "Сейчас проверю тариф.\n<tool_call><function=bash><parameter=command>ls -la</parameter></function></tool_call>\nГотово.";
        let (calls, cleaned) = extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
        assert_eq!(cleaned, "Сейчас проверю тариф.\n\nГотово.");
    }

    #[test]
    fn parses_json_bodies() {
        let text = "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}</tool_call>";
        let (calls, _) = extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn parses_named_function_tag_and_unterminated_block() {
        let text = "<tool_call><function name=\"send_file\"><parameter=path>x.pdf</parameter></function></tool_call><tool_call><function=webfetch><parameter=url>https://e.ru";
        let (calls, cleaned) = extract(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "send_file");
        assert_eq!(calls[1].name, "webfetch");
        assert_eq!(calls[1].arguments["url"], "https://e.ru");
        assert_eq!(cleaned, "");
    }

    #[test]
    fn ignores_plain_text_and_unknown_blocks() {
        assert_eq!(extract("просто ответ").0.len(), 0);
        assert_eq!(extract("просто ответ").1, "просто ответ");
        let (calls, _) = extract("<tool_call>мусор без функции</tool_call>");
        assert!(calls.is_empty());
    }

    #[test]
    fn strip_removes_markup_from_reasoning() {
        let reasoning = "Думаю.\n<tool_call><function=bash><parameter=command>pwd</parameter></tool_call>\nЕщё думаю.";
        assert_eq!(strip(reasoning), "Думаю.\n\nЕщё думаю.");
    }
}
