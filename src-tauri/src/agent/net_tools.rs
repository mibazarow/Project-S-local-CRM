// Network tools (webfetch + websearch) for both catalogs. ureq only, no new
// deps: redirects are followed manually so the shared SSRF guard (mcp::check_url)
// re-checks every hop, the body is capped at MAX_FETCH_BYTES, and HTML is
// reduced to readable text by a small state machine (script/style/comments
// dropped, block tags become line breaks, common entities decoded, whitespace
// collapsed). websearch scrapes DuckDuckGo's HTML endpoint and resolves its
// /l/?uddg= redirect links back to the real target URLs.
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::mcp::check_url;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FETCH_BYTES: u64 = 2 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 5;
const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_MAX_CHARS: usize = 200_000;
const DDG_BASE: &str = "https://html.duckduckgo.com/html/";
// DDG's HTML endpoint answers plain-text bots poorly without a browser UA.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

struct Fetched {
    final_url: String,
    content_type: String,
    body: Vec<u8>,
    truncated: bool,
}

// GET with manual redirect following: every hop (including Location targets)
// goes through the shared SSRF guard before a single packet is sent.
fn fetch(url: &str, connect: Duration, read: Duration) -> std::result::Result<Fetched, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(connect)
        .timeout_read(read)
        .redirects(0)
        .build();
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        check_url(&url).map_err(|e| e.to_string())?;
        // redirects(0): 3xx arrives as a plain response, not an error
        let resp = match agent.get(&url).set("User-Agent", USER_AGENT).call() {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let brief: String = resp.into_string().unwrap_or_default().chars().take(200).collect();
                return Err(format!("HTTP {status} from {url}: {brief}"));
            }
            Err(e) => return Err(format!("request to {url} failed: {e}")),
        };
        if (300..400).contains(&resp.status()) {
            let Some(location) = resp.header("location") else {
                return Err(format!(
                    "redirect {} from {url} has no location header",
                    resp.status()
                ));
            };
            let Some(next) = resolve_url(&url, location.trim()) else {
                return Err(format!("unresolvable redirect target {location:?} from {url}"));
            };
            url = next;
            continue;
        }
        let content_type = resp.content_type().to_string();
        let mut body = Vec::new();
        if let Err(e) = (&mut resp.into_reader())
            .take(MAX_FETCH_BYTES + 1)
            .read_to_end(&mut body)
        {
            return Err(format!("reading response body from {url} failed: {e}"));
        }
        let truncated = body.len() as u64 > MAX_FETCH_BYTES;
        body.truncate(MAX_FETCH_BYTES as usize);
        return Ok(Fetched {
            final_url: url,
            content_type,
            body,
            truncated,
        });
    }
    Err(format!("refused: more than {MAX_REDIRECTS} redirects"))
}

// Resolves a redirect Location against the current URL without a URL crate:
// absolute, scheme-relative, root-relative and path-relative forms.
fn resolve_url(base: &str, loc: &str) -> Option<String> {
    if loc.is_empty() {
        return None;
    }
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(rest) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if let Some(path) = loc.strip_prefix('/') {
        return Some(format!("{scheme}://{authority}/{path}"));
    }
    let path = rest.split_once('/').map_or("", |(_, path)| path);
    let dir = match path.rfind('/') {
        Some(i) => &path[..i + 1],
        None => "",
    };
    Some(format!("{scheme}://{authority}/{dir}{loc}"))
}

fn is_pass_through(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    base == "application/json"
        || base.ends_with("+json")
        || base.starts_with("text/")
        || base == "application/xml"
        || base.ends_with("+xml")
}

fn is_html(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    base == "text/html" || base == "application/xhtml+xml"
}

const BLOCK_TAGS: [&str; 29] = [
    "p", "div", "br", "li", "ul", "ol", "dl", "dt", "dd", "h1", "h2", "h3", "h4", "h5", "h6",
    "tr", "table", "thead", "tbody", "tfoot", "section", "article", "header", "footer", "aside",
    "nav", "blockquote", "pre", "title",
];

fn tag_name(inner: &str) -> String {
    let inner = inner.trim_start_matches('/').trim_end_matches('/');
    let end = inner.find(|c: char| !c.is_alphabetic()).unwrap_or(inner.len());
    inner[..end].to_lowercase()
}

// HTML -> readable plain text: comments, script/style/head elements are
// dropped, block-level tags become line breaks, other tags become spaces.
pub(crate) fn html_to_text(html: &str) -> String {
    let b = html.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            let start = i;
            while i < b.len() && b[i] != b'<' {
                i += 1;
            }
            out.push_str(&html[start..i]);
            continue;
        }
        if b[i..].starts_with(b"<!--") {
            i = html[i + 4..].find("-->").map_or(b.len(), |rel| i + 4 + rel + 3);
            continue;
        }
        let Some(gt) = html[i + 1..].find('>') else {
            break;
        };
        let inner = &html[i + 1..i + 1 + gt];
        let name = tag_name(inner);
        // Opening script/style: jump straight past the matching close tag.
        // The closing tag itself then falls through and is skipped normally.
        if (name == "script" || name == "style") && !inner.starts_with('/') {
            let close = format!("</{name}");
            let from = i + gt + 2;
            i = html[from..]
                .to_lowercase()
                .find(&close)
                .map_or(b.len(), |rel| from + rel);
            continue;
        }
        out.push(if BLOCK_TAGS.contains(&name.as_str()) { '\n' } else { ' ' });
        i += gt + 2;
    }
    collapse_lines(&decode_entities(&out))
}

const ENTITIES: [(&str, &str); 36] = [
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{a0}"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("hellip", "…"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("deg", "°"),
    ("plusmn", "±"),
    ("frac12", "½"),
    ("frac14", "¼"),
    ("frac34", "¾"),
    ("sup1", "¹"),
    ("sup2", "²"),
    ("sup3", "³"),
    ("times", "×"),
    ("divide", "÷"),
    ("middot", "·"),
    ("bull", "•"),
    ("dagger", "†"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("sect", "§"),
];

pub(crate) fn decode_entities(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' {
            let start = i;
            while i < b.len() && b[i] != b'&' {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        let Some(end) = (i + 2..(i + 12).min(b.len())).find(|&j| b[j] == b';') else {
            out.push('&');
            i += 1;
            continue;
        };
        let body = &text[i + 1..end];
        let decoded = if let Some(digits) = body.strip_prefix('#') {
            let num = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse::<u32>().ok(),
            };
            num.and_then(char::from_u32).map(|c| c.to_string())
        } else {
            ENTITIES
                .iter()
                .find(|(name, _)| *name == body)
                .map(|(_, value)| value.to_string())
        };
        match decoded {
            Some(value) => {
                out.push_str(&value);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

// One line per source line: trim, collapse inner whitespace, drop empties.
fn collapse_lines(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.split_whitespace().collect::<Vec<&str>>().join(" ");
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            for c in chars.by_ref() {
                if c == '>' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn clean_inline(s: &str) -> String {
    let text = decode_entities(&strip_tags(s));
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

// DDG result blocks: <a class="result__a" href="...">title</a> plus the
// matching <a class="result__snippet">snippet</a> in the same result.
pub(crate) fn parse_ddg_results(html: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(found) = html[pos..].find("class=\"result__a") {
        let anchor = pos + found;
        let Some(rel) = html[anchor..].find("href=\"") else {
            break;
        };
        let href_start = anchor + rel + 6;
        let Some(rel) = html[href_start..].find('"') else {
            break;
        };
        let href = &html[href_start..href_start + rel];
        let Some(rel) = html[href_start..].find('>') else {
            break;
        };
        let text_start = href_start + rel + 1;
        let Some(rel) = html[text_start..].find("</a>") else {
            break;
        };
        let title = clean_inline(&html[text_start..text_start + rel]);
        let snippet = snippet_after(html, text_start + rel);
        out.push(json!({
            "title": title,
            "url": resolve_ddg_url(href),
            "snippet": snippet,
        }));
        pos = text_start + rel;
    }
    out
}

fn snippet_after(html: &str, from: usize) -> String {
    let Some(rel) = html[from..].find("result__snippet") else {
        return String::new();
    };
    let at = from + rel;
    let Some(rel) = html[at..].find('>') else {
        return String::new();
    };
    let start = at + rel + 1;
    let Some(rel) = html[start..].find("</a>") else {
        return String::new();
    };
    clean_inline(&html[start..start + rel])
}

// /l/?uddg=<percent-encoded target>&rut=... -> the real target URL.
fn resolve_ddg_url(href: &str) -> String {
    if let Some(rest) = href.split_once("uddg=").map(|(_, rest)| rest) {
        let encoded = rest.split('&').next().unwrap_or("");
        return percent_decode(encoded).unwrap_or_else(|| encoded.to_string());
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https:{rest}");
    }
    href.to_string()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(enc: &str) -> Option<String> {
    let b = enc.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hi = hex_val(*b.get(i + 1)?)?;
                let lo = hex_val(*b.get(i + 2)?)?;
                bytes.push(hi << 4 | lo);
                i += 3;
            }
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            c => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(bytes).ok()
}

fn form_encode(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for c in q.as_bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*c as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{c:02X}")),
        }
    }
    out
}

pub struct WebfetchTool {
    connect: Duration,
    read: Duration,
}

impl WebfetchTool {
    pub fn new() -> Self {
        WebfetchTool {
            connect: CONNECT_TIMEOUT,
            read: READ_TIMEOUT,
        }
    }

    pub fn with_timeouts(connect: Duration, read: Duration) -> Self {
        WebfetchTool { connect, read }
    }
}

impl Default for WebfetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebfetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> &str {
        "Fetch an http(s) page and return its readable plain text (HTML tags, scripts and styles are stripped; JSON and text pass through). Follows redirects, caps the download at 2 MiB and the text at max_chars, and refuses hosts on the SSRF deny list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "max_chars": { "type": "number" }
            },
            "required": ["url"],
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
        30_000
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let url = args["url"].as_str().unwrap_or_default().trim();
        if url.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "url is empty"));
        }
        if let Err(e) = check_url(url) {
            return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e.to_string()));
        }
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_MAX_CHARS as f64)
            .clamp(1.0, MAX_MAX_CHARS as f64) as usize;
        let fetched = match fetch(url, self.connect, self.read) {
            Ok(fetched) => fetched,
            // a redirect hop onto a denied host keeps the same verdict as
            // the initial URL check
            Err(e) if e.contains("ssrf") => {
                return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e))
            }
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        let raw = String::from_utf8_lossy(&fetched.body).into_owned();
        let text = if is_html(&fetched.content_type) {
            html_to_text(&raw)
        } else if is_pass_through(&fetched.content_type) {
            raw
        } else {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "unsupported content type {:?}; expected html, json or text",
                    fetched.content_type
                ),
            ));
        };
        let mut truncated = fetched.truncated;
        let text: String = if text.chars().count() > max_chars {
            truncated = true;
            text.chars().take(max_chars).collect()
        } else {
            text
        };
        Ok(ToolResult::ok(json!({
            "url": url,
            "finalUrl": fetched.final_url,
            "contentType": fetched.content_type,
            "text": text,
            "truncated": truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct WebsearchTool {
    endpoint: String,
    connect: Duration,
    read: Duration,
}

impl WebsearchTool {
    pub fn new() -> Self {
        WebsearchTool {
            endpoint: DDG_BASE.to_string(),
            connect: CONNECT_TIMEOUT,
            read: READ_TIMEOUT,
        }
    }

    // Test seam: point the scraper at a local mock instead of DuckDuckGo.
    pub fn with_endpoint(endpoint: &str) -> Self {
        WebsearchTool {
            endpoint: endpoint.to_string(),
            connect: CONNECT_TIMEOUT,
            read: READ_TIMEOUT,
        }
    }
}

impl Default for WebsearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebsearchTool {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> &str {
        "Web search via DuckDuckGo's HTML endpoint (no API key): returns results with title, resolved url and snippet. limit defaults to 8, max 20."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let query = args["query"].as_str().unwrap_or_default().trim();
        if query.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "query is empty"));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(8.0)
            .clamp(1.0, 20.0) as usize;
        let url = format!("{}?q={}", self.endpoint, form_encode(query));
        let fetched = match fetch(&url, self.connect, self.read) {
            Ok(fetched) => fetched,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        let html = String::from_utf8_lossy(&fetched.body);
        let mut results = parse_ddg_results(&html);
        results.truncate(limit);
        if results.is_empty() {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                "search returned no results or the provider changed",
            ));
        }
        Ok(ToolResult::ok(json!({ "query": query, "results": results }))
            .capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::mcp::mock_server::{respond, Handler, MockServer};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Instant;

    const DDG_FIXTURE: &str = r#"<html><body>
    <div class="result results_links results_links_deep web-result">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&rut=deadbeef">Example &amp; Docs</a>
        </h2>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&rut=deadbeef">Docs for the <b>example</b>&nbsp;tool &mdash; plain text.</a>
      </div>
    </div>
    <div class="result">
      <a class="result__a" href="https://direct.example.org/page">Direct hit</a>
      <a class="result__snippet">second &lt;snippet&gt;</a>
    </div>
    </body></html>"#;

    fn mock(handler: Handler) -> MockServer {
        MockServer::start(handler)
    }

    #[test]
    fn html_to_text_strips_scripts_styles_comments_and_collapses_whitespace() {
        let html = "<html><head><title>T</title><style>a{color:red}</style></head>\
             <body><script>var x = \"<b>\";</script>\
             <!-- hidden note -->\
             <h1>Title</h1><p>First   line<br>second&nbsp;line</p>\
             <div>Third &amp; done</div></body></html>";
        assert_eq!(
            html_to_text(html),
            "T\nTitle\nFirst line\nsecond line\nThird & done"
        );
    }

    #[test]
    fn decode_entities_named_numeric_and_unknown() {
        assert_eq!(decode_entities("a &amp; b &lt;tag&gt;"), "a & b <tag>");
        assert_eq!(decode_entities("&#65;&#x42;&#38;"), "AB&");
        assert_eq!(decode_entities("&#x41;"), "A");
        assert_eq!(decode_entities("x &nbsp; y — z"), "x \u{a0} y — z");
        // unknown or unterminated entities stay verbatim
        assert_eq!(decode_entities("&bogus; &amp"), "&bogus; &amp");
        assert_eq!(decode_entities("&#xZZ;"), "&#xZZ;");
    }

    #[test]
    fn parse_ddg_results_resolves_uddg_and_direct_links() {
        let results = parse_ddg_results(DDG_FIXTURE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], "Example & Docs");
        assert_eq!(results[0]["url"], "https://example.com/docs");
        assert_eq!(
            results[0]["snippet"].as_str().unwrap(),
            "Docs for the example tool — plain text."
        );
        assert_eq!(results[1]["url"], "https://direct.example.org/page");
        assert_eq!(results[1]["snippet"], "second <snippet>");
    }

    #[test]
    fn resolve_url_handles_absolute_root_relative_and_relative_locations() {
        assert_eq!(
            resolve_url("http://a.example/x/y", "https://b.example/z").as_deref(),
            Some("https://b.example/z")
        );
        assert_eq!(
            resolve_url("https://a.example/x/y", "/final").as_deref(),
            Some("https://a.example/final")
        );
        assert_eq!(
            resolve_url("http://a.example/x/y", "next/page").as_deref(),
            Some("http://a.example/x/next/page")
        );
        assert_eq!(
            resolve_url("https://a.example/x", "//cdn.example/z").as_deref(),
            Some("https://cdn.example/z")
        );
        assert_eq!(resolve_url("https://a.example/x", ""), None);
    }

    #[test]
    fn percent_and_form_encoding_round_trip_queries() {
        assert_eq!(percent_decode("a%20b%21c"), Some("a b!c".to_string()));
        assert_eq!(percent_decode("%D0%BA"), Some("к".to_string()));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(form_encode("hello world &к"), "hello+world+%26%D0%BA");
    }

    #[test]
    fn webfetch_refuses_denied_hosts_before_connecting() {
        let tool = WebfetchTool::new();
        let r = tool
            .execute(&json!({ "url": "http://169.254.169.254/latest/meta-data/" }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.as_ref().unwrap().message.contains("ssrf"));
    }

    #[test]
    fn webfetch_rejects_bad_urls_and_empty_urls() {
        let tool = WebfetchTool::new();
        let r = tool.execute(&json!({ "url": "ftp://example.com" }), &ToolCtx::default()).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        let r = tool.execute(&json!({ "url": "   " }), &ToolCtx::default()).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
    }

    #[test]
    fn webfetch_follows_redirects_and_guards_every_hop() {
        let server = mock(Arc::new(
            |stream: &mut std::net::TcpStream, req| match req.path.as_str() {
                "/start" => respond(stream, 302, "text/plain", "", &[("Location", "/final")]),
                "/to-metadata" => respond(
                    stream,
                    302,
                    "text/plain",
                    "",
                    &[("Location", "http://169.254.169.254/late")],
                ),
                _ => respond(stream, 200, "text/html; charset=utf-8", "<html><body><h1>Final</h1><p>body   text</p></body></html>", &[]),
            },
        ));

        let tool = WebfetchTool::new();
        let r = tool
            .execute(&json!({ "url": format!("{}/start", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["finalUrl"], format!("{}/final", server.url_base));
        assert_eq!(data["text"], "Final\nbody text");
        assert_eq!(data["truncated"], false);

        // a redirect hop to a denied host is refused, never fetched
        let r = tool
            .execute(&json!({ "url": format!("{}/to-metadata", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.as_ref().unwrap().message.contains("ssrf"));
    }

    #[test]
    fn webfetch_caps_body_at_two_mebibytes_and_text_at_max_chars() {
        let big = "x".repeat(3 * 1024 * 1024);
        let server = mock(Arc::new(move |stream: &mut std::net::TcpStream, _req| {
            // No set_nonblocking(false) workaround here: MockServer already
            // forces accepted sockets to blocking mode with I/O timeouts,
            // so the multi-MB write_all below is a plain blocking write.
            respond(stream, 200, "text/plain", &big, &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/big", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["truncated"], true);
        assert_eq!(data["text"].as_str().unwrap().chars().count(), DEFAULT_MAX_CHARS);

        // explicit max_chars is honored; a request beyond the tool's
        // max_result_len comes back as limit_exceeded from capped()
        let r = WebfetchTool::new()
            .execute(
                &json!({ "url": format!("{}/big", server.url_base), "max_chars": 100 }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["text"].as_str().unwrap().chars().count(), 100);
        let r = WebfetchTool::new()
            .execute(
                &json!({ "url": format!("{}/big", server.url_base), "max_chars": 1_000_000 }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "limit_exceeded");
    }

    #[test]
    fn webfetch_rejects_binary_content_types() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "image/png", "not really png", &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/img", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert!(r.error.as_ref().unwrap().message.contains("unsupported content type"));
    }

    #[test]
    fn webfetch_passes_json_through_without_conversion() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "application/json", "{\"a\": 1}", &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/api", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["text"], "{\"a\": 1}");
    }

    #[test]
    fn webfetch_read_timeout_surfaces_as_execution_error() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\n")?;
            std::thread::sleep(Duration::from_secs(2));
            stream.write_all(b"never arrives in time")
        }));
        let tool = WebfetchTool::with_timeouts(Duration::from_millis(500), Duration::from_millis(300));
        let started = Instant::now();
        let r = tool
            .execute(&json!({ "url": format!("{}/slow", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert!(r.error.as_ref().unwrap().message.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn websearch_parses_results_from_the_endpoint_and_applies_limit() {
        let fixture = DDG_FIXTURE.to_string();
        let server = mock(Arc::new(move |stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "text/html", &fixture, &[])
        }));
        let tool = WebsearchTool::with_endpoint(&server.url_base);
        let r = tool
            .execute(&json!({ "query": "example tool" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["query"], "example tool");
        let results = data["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["url"], "https://example.com/docs");
        let log = server.request_log();
        assert_eq!(log[0].path, "/?q=example+tool");

        let r = tool
            .execute(&json!({ "query": "example", "limit": 1 }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.data.unwrap()["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn websearch_reports_execution_error_when_nothing_parses() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "text/html", "<html><body>anomaly</body></html>", &[])
        }));
        let r = WebsearchTool::with_endpoint(&server.url_base)
            .execute(&json!({ "query": "anything" }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert_eq!(
            r.error.as_ref().unwrap().message,
            "search returned no results or the provider changed"
        );
    }

    #[test]
    fn websearch_rejects_empty_query() {
        let r = WebsearchTool::new()
            .execute(&json!({ "query": "  " }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
    }

    #[test]
    fn schemas_validate_accept_and_reject_calls() {
        let fetch_schema = WebfetchTool::new().parameters();
        assert!(validate_args(&fetch_schema, &json!({ "url": "https://example.com" })).is_ok());
        assert!(validate_args(
            &fetch_schema,
            &json!({ "url": "https://example.com", "max_chars": 1000 })
        )
        .is_ok());
        assert!(validate_args(&fetch_schema, &json!({ "max_chars": 10 })).is_err());
        assert!(validate_args(&fetch_schema, &json!({ "url": 5 })).is_err());
        assert!(validate_args(
            &fetch_schema,
            &json!({ "url": "https://example.com", "extra": 1 })
        )
        .is_err());

        let search_schema = WebsearchTool::new().parameters();
        assert!(validate_args(&search_schema, &json!({ "query": "q" })).is_ok());
        assert!(validate_args(&search_schema, &json!({ "query": "q", "limit": 5 })).is_ok());
        assert!(validate_args(&search_schema, &json!({ "limit": 5 })).is_err());
        assert!(validate_args(&search_schema, &json!({ "query": "q", "limit": "x" })).is_err());
    }

    #[test]
    fn policies_are_readonly_skip() {
        for tool in [Box::new(WebfetchTool::new()) as Box<dyn Tool>, Box::new(WebsearchTool::new())] {
            assert_eq!(tool.class(), ToolClass::ReadOnly);
            assert_eq!(tool.policy(), PermissionPolicy::Skip);
            assert!(tool.danger_reason(&json!({}), &ToolCtx::default()).is_none());
        }
    }

    #[test]
    #[ignore = "live network: fetch example.com"]
    fn live_fetch_example_com() {
        let r = WebfetchTool::new()
            .execute(&json!({ "url": "https://example.com/" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert!(data["text"].as_str().unwrap().contains("example"));
    }

    #[test]
    #[ignore = "live network: real DuckDuckGo search"]
    fn live_search_finds_results() {
        let r = WebsearchTool::new()
            .execute(&json!({ "query": "tauri framework", "limit": 3 }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(!r.data.unwrap()["results"].as_array().unwrap().is_empty());
    }
}

