// Inline tool-call parser.
//
// Most OpenAI-compatible providers return tool calls in the native `tool_calls`
// field, but some gateways and finetunes write the call as text instead. This
// module recognises the dialects we have seen in the wild and turns them into
// real ToolCalls, and strips their markup from the visible text.
//
// Dialects:
//   1. <tool_call><function=name><parameter=k>v</parameter></tool_call>
//      (also without the closing </function>/</parameter>, as models often drop
//      them at the turn boundary)
//   2. <tool_call>{"name": "name", "arguments": {...}}</tool_call>
//   3. <|tool_call|> … <|/tool_call|> with either body form
//   4. <function_calls><invoke name="name"><parameter name="k">v</parameter>
//      </invoke></function_calls>
//   5. A fenced ```json {"name": …, "arguments": …} ``` block
//   6. A bare JSON object with "name" + "arguments"/"parameters"
//
// Dialects 1-4 are unambiguous: the markup itself says "this is a tool call",
// so the call is always produced (an unknown name then surfaces as the loop's
// `unknown tool` error, which teaches the model). Dialects 5-6 look like prose
// JSON, so they are accepted only when the name matches a registered tool.
use crate::llm::types::ToolCall;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";
const PIPE_OPEN: &str = "<|tool_call|>";
const PIPE_CLOSE: &str = "<|/tool_call|>";
const FUNCS_OPEN: &str = "<function_calls>";
const FUNCS_CLOSE: &str = "</function_calls>";

/// What one assistant message yielded.
#[derive(Default)]
pub(crate) struct Parsed {
    pub calls: Vec<ToolCall>,
    pub cleaned: String,
}

/// Parse every known dialect out of `text`. `known` gates the ambiguous
/// dialects (fenced/bare JSON); pass an empty set to disable them.
pub(crate) fn parse(text: &str, known: &BTreeSet<String>) -> Parsed {
    let mut out = Parsed {
        cleaned: text.to_string(),
        ..Parsed::default()
    };
    if text.is_empty() {
        return out;
    }
    let original = out.cleaned.clone();
    for (open, close, kind) in [
        (CALL_OPEN, CALL_CLOSE, Body::Mixed),
        (PIPE_OPEN, PIPE_CLOSE, Body::Mixed),
        (FUNCS_OPEN, FUNCS_CLOSE, Body::Invoke),
    ] {
        extract_blocks(&mut out, open, close, kind, known);
    }
    extract_bare_functions(&mut out, known);
    if out.calls.is_empty() {
        extract_fenced_json(&mut out, known);
        extract_bare_json(&mut out, known);
    }
    // Only normalise whitespace when something was actually removed: a plain
    // answer must come back byte-for-byte (streaming readability).
    if out.cleaned != original {
        out.cleaned = out.cleaned.trim().to_string();
    }
    out
}

/// Strip every known dialect without interpreting it (display, reasoning).
pub(crate) fn clean(text: &str) -> String {
    let empty = BTreeSet::new();
    parse(text, &empty).cleaned
}

#[derive(Clone, Copy)]
enum Body {
    /// `<function=…>…</function>` or a JSON object.
    Mixed,
    /// `<invoke name="…">…</invoke>`.
    Invoke,
}

fn extract_blocks(
    out: &mut Parsed,
    open: &str,
    close: &str,
    kind: Body,
    known: &BTreeSet<String>,
) {
    while let Some(start) = out.cleaned.find(open) {
        let after = out.cleaned[start + open.len()..].to_string();
        let (body, tail) = match after.find(close) {
            Some(end) => (after[..end].to_string(), after[end + close.len()..].to_string()),
            // Unterminated block: the rest of the message is the call.
            None => (after, String::new()),
        };
        let index = out.calls.len();
        match kind {
            Body::Mixed => {
                if let Some(call) = parse_mixed_body(&body, index, known) {
                    record(out, call);
                }
            }
            Body::Invoke => {
                for call in parse_invoke_body(&body, index, known) {
                    record(out, call);
                }
            }
        }
        let before = out.cleaned[..start].to_string();
        out.cleaned = format!("{before}{tail}");
    }
}

fn record(out: &mut Parsed, call: ToolCall) {
    out.calls.push(call);
}

fn parse_mixed_body(body: &str, index: usize, known: &BTreeSet<String>) -> Option<ToolCall> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if body.starts_with('{') {
        return parse_json_call(body, index, known, true);
    }
    let (name, rest) = parse_function_open(body)?;
    let arguments = parse_xml_parameters(rest);
    Some(build(index, &name, arguments))
}

// <invoke name="read_file"><parameter name="path">a.txt</parameter></invoke>
fn parse_invoke_body(body: &str, index: usize, known: &BTreeSet<String>) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("<invoke") {
        let after = &rest[open + "<invoke".len()..];
        let Some(name) = attr(after, "name") else {
            rest = after;
            continue;
        };
        let Some(gt) = after.find('>') else {
            break;
        };
        let inner = &after[gt + 1..];
        let end = inner.find("</invoke>").unwrap_or(inner.len());
        let arguments = parse_named_parameters(&inner[..end]);
        calls.push(build(index + calls.len(), &name, arguments));
        rest = &inner[end..];
    }
    let _ = known;
    calls
}

// <function=webfetch> / <function name="webfetch"> / <function>webfetch</function>
fn parse_function_open(body: &str) -> Option<(String, &str)> {
    let open = body.find("<function")?;
    let after = &body[open + "<function".len()..];
    let (name, rest) = if let Some(rest) = after.strip_prefix('=') {
        let end = rest.find('>')?;
        (rest[..end].trim().to_string(), &rest[end + 1..])
    } else if let Some(name) = attr(after, "name") {
        let end = after.find('>')?;
        (name, &after[end + 1..])
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

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let at = tag.find(&needle)?;
    let rest = &tag[at + needle.len()..];
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    match quote {
        Some(q) => {
            let rest = &rest[q.len_utf8()..];
            let end = rest.find(q)?;
            Some(rest[..end].trim().to_string())
        }
        None => {
            let end = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/')?;
            Some(rest[..end].trim().to_string())
        }
    }
}

// <parameter=key>value</parameter>; a missing close tag ends the value at the
// next parameter or at the end of the block.
fn parse_xml_parameters(body: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let mut rest = body;
    while let Some(open) = rest.find("<parameter") {
        let after = &rest[open + "<parameter".len()..];
        let (key, tail) = if let Some(rest_after) = after.strip_prefix('=') {
            let Some(end) = rest_after.find('>') else { break };
            (rest_after[..end].trim().to_string(), &rest_after[end + 1..])
        } else if let Some(key) = attr(after, "name") {
            let Some(end) = after.find('>') else { break };
            (key, &after[end + 1..])
        } else {
            rest = after;
            continue;
        };
        let (value, next) = match tail.find("</parameter>") {
            Some(close) => (&tail[..close], &tail[close + "</parameter>".len()..]),
            None => {
                let stop = tail.find("<parameter").unwrap_or(tail.len());
                (&tail[..stop], &tail[stop..])
            }
        };
        if !key.is_empty() {
            out.insert(key, typed_value(value));
        }
        rest = next;
    }
    out
}

// <parameter name="k">value</parameter> (invoke dialect)
fn parse_named_parameters(body: &str) -> Map<String, Value> {
    parse_xml_parameters(body)
}

// Values arrive as text. Structured values (arrays/objects) are common for
// list parameters, so a JSON-looking payload is decoded instead of kept as a
// string that would then fail schema validation.
fn typed_value(raw: &str) -> Value {
    let text = raw.trim();
    if text.starts_with('{') || text.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return value;
        }
    }
    Value::String(text.to_string())
}

/// A `<function=name>…</function>` written without the `<tool_call>` wrapper.
/// Unambiguous enough to accept, but only for a registered name when we can
/// check it.
fn extract_bare_functions(out: &mut Parsed, known: &BTreeSet<String>) {
    while let Some(start) = out.cleaned.find("<function") {
        let rest = out.cleaned[start..].to_string();
        let Some((name, after_open)) = parse_function_open(&rest) else {
            break;
        };
        if !known.is_empty() && !known.contains(&name) {
            // Not a tool: leave the text alone so it can be read as prose.
            break;
        }
        let has_close = after_open.contains("</function>");
        let body_len = after_open.find("</function>").unwrap_or(after_open.len());
        let body = after_open[..body_len].to_string();
        let consumed = rest.len() - after_open.len()
            + body_len
            + if has_close { "</function>".len() } else { 0 };
        let index = out.calls.len();
        record(out, build(index, &name, parse_xml_parameters(&body)));
        let before = out.cleaned[..start].to_string();
        out.cleaned = format!("{before}{}", &rest[consumed..]);
    }
}

fn extract_fenced_json(out: &mut Parsed, known: &BTreeSet<String>) {
    let mut rest = out.cleaned.clone();
    while let Some(open) = rest.find("```") {
        let after = rest[open + 3..].to_string();
        let after_body = after.strip_prefix("json").unwrap_or(&after).to_string();
        let Some(close) = after_body.find("```") else {
            break;
        };
        let body = after_body[..close].trim();
        if let Some(call) = parse_json_call(body, out.calls.len(), known, false) {
            record(out, call);
            let tail = &after_body[close + 3..];
            rest = format!("{}{tail}", &rest[..open]);
            out.cleaned = rest.clone();
        } else {
            rest = after_body[close + 3..].to_string();
        }
    }
}

fn extract_bare_json(out: &mut Parsed, known: &BTreeSet<String>) {
    let trimmed = out.cleaned.trim();
    if !trimmed.starts_with('{') {
        return;
    }
    if let Some(call) = parse_json_call(trimmed, out.calls.len(), known, false) {
        record(out, call);
        out.cleaned.clear();
    }
}

fn parse_json_call(
    body: &str,
    index: usize,
    known: &BTreeSet<String>,
    _unambiguous: bool,
) -> Option<ToolCall> {
    let body = body
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim();
    let value: Value = serde_json::from_str(body).ok()?;
    let name = value.get("name").and_then(Value::as_str)?.trim();
    if name.is_empty() {
        return None;
    }
    // JSON in prose is ambiguous: only a registered tool name turns it into a
    // call. (Empty `known` means the caller cannot check — refuse.)
    if known.is_empty() || !known.contains(name) {
        return None;
    }
    let arguments = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let arguments = match arguments {
        Value::String(raw) => serde_json::from_str::<Value>(&raw).ok()?,
        other => other,
    };
    Some(build(index, name, match arguments {
        Value::Object(map) => map,
        _ => Map::new(),
    }))
}

fn build(index: usize, name: &str, arguments: Map<String, Value>) -> ToolCall {
    ToolCall::new(
        format!("call_inline_{index}"),
        name.to_string(),
        Value::Object(arguments).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    // The exact shape reported from the wild: no </function>, sometimes no
    // </parameter>, several blocks in one answer.
    const REPORTED: &str = "<tool_call>\n<function=webfetch>\n<parameter=url>\nhttps://plusvibeapi.ru/model/deepseek-v4.1-flash-1017\n\n</parameter>\n</tool_call><tool_call>\n<function=http_request>\n<parameter=method>\nGET\n\n<parameter=url>\nhttps://plusvibeapi.ru/model/x\n\n</tool_call>";

    #[test]
    fn parses_the_reported_dialect() {
        let parsed = parse(REPORTED, &known(&["webfetch", "http_request"]));
        assert_eq!(parsed.calls.len(), 2);
        assert_eq!(parsed.calls[0].name, "webfetch");
        assert_eq!(
            parsed.calls[0].arguments["url"],
            "https://plusvibeapi.ru/model/deepseek-v4.1-flash-1017"
        );
        assert_eq!(parsed.calls[1].name, "http_request");
        assert_eq!(parsed.calls[1].arguments["method"], "GET");
        assert_eq!(parsed.calls[1].arguments["url"], "https://plusvibeapi.ru/model/x");
        assert_eq!(parsed.cleaned, "");
    }

    #[test]
    fn keeps_the_prose_around_the_blocks() {
        let text = "Сейчас проверю тариф.\n<tool_call><function=bash><parameter=command>ls -la</parameter></function></tool_call>\nГотово.";
        let parsed = parse(text, &known(&["bash"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "bash");
        assert_eq!(parsed.calls[0].arguments["command"], "ls -la");
        assert_eq!(parsed.cleaned, "Сейчас проверю тариф.\n\nГотово.");
    }

    #[test]
    fn parses_json_inside_the_wrapper() {
        let text = "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}</tool_call>";
        let parsed = parse(text, &known(&["read_file"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn parses_pipe_tags_and_invoke_dialect() {
        let pipe = "<|tool_call|><function=send_file><parameter=path>x.pdf</parameter></function><|/tool_call|>";
        let parsed = parse(pipe, &known(&["send_file"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "send_file");

        let invoke = "<function_calls><invoke name=\"fs_write\"><parameter name=\"path\">a.txt</parameter><parameter name=\"content\">hi</parameter></invoke></function_calls>";
        let parsed = parse(invoke, &known(&["fs_write"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].arguments["path"], "a.txt");
        assert_eq!(parsed.calls[0].arguments["content"], "hi");
        assert_eq!(parsed.cleaned, "");
    }

    #[test]
    fn decodes_structured_parameter_values() {
        let text = "<tool_call><function=trash_move><parameter=paths>[\"a\",\"b\"]</parameter></function></tool_call>";
        let parsed = parse(text, &known(&["trash_move"]));
        assert_eq!(parsed.calls[0].arguments["paths"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn accepts_fenced_and_bare_json_only_for_known_tools() {
        let fenced = "```json\n{\"name\":\"webfetch\",\"arguments\":{\"url\":\"https://e.ru\"}}\n```";
        let parsed = parse(fenced, &known(&["webfetch"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.cleaned, "");

        let bare = "{\"name\":\"webfetch\",\"arguments\":{\"url\":\"https://e.ru\"}}";
        assert_eq!(parse(bare, &known(&["webfetch"])).calls.len(), 1);
        // Unknown name: prose, not a call.
        assert_eq!(parse(bare, &known(&["bash"])).calls.len(), 0);
        // No catalog to check against: refuse the ambiguous forms.
        assert_eq!(parse(bare, &BTreeSet::new()).calls.len(), 0);
    }

    #[test]
    fn accepts_a_bare_function_tag_only_for_known_tools() {
        let text = "<function=webfetch><parameter=url>https://e.ru</parameter></function>";
        assert_eq!(parse(text, &known(&["webfetch"])).calls.len(), 1);
        let prose = "Пример: <function=example>…</function> — вот так.";
        let parsed = parse(prose, &known(&["webfetch"]));
        assert!(parsed.calls.is_empty());
        assert!(parsed.cleaned.contains("<function=example>"));
    }

    #[test]
    fn unknown_wrapped_names_still_surface_as_calls() {
        let text = "<tool_call><function=no_such_tool><parameter=x>1</parameter></tool_call>";
        let parsed = parse(text, &known(&["bash"]));
        assert_eq!(parsed.calls.len(), 1);
        assert_eq!(parsed.calls[0].name, "no_such_tool");
    }

    #[test]
    fn ignores_plain_text_and_unknown_blocks() {
        assert_eq!(parse("просто ответ", &known(&[])).calls.len(), 0);
        assert_eq!(parse("просто ответ", &known(&[])).cleaned, "просто ответ");
        let parsed = parse("<tool_call>мусор без функции</tool_call>", &known(&[]));
        assert!(parsed.calls.is_empty());
        assert_eq!(parsed.cleaned, "");
    }

    #[test]
    fn clean_strips_markup_from_reasoning() {
        let reasoning = "Думаю.\n<tool_call><function=bash><parameter=command>pwd</parameter></tool_call>\nЕщё думаю.";
        assert_eq!(clean(reasoning), "Думаю.\n\nЕщё думаю.");
        assert_eq!(clean("без разметки"), "без разметки");
    }
}
