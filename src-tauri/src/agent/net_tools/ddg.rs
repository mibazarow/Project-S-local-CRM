// DuckDuckGo HTML endpoint parsing for websearch: result blocks, the
// /l/?uddg= redirect unwrapping and the two small encoders (percent decoding
// of the wrapped targets, form encoding of the query). websearch scrapes
// DuckDuckGo's HTML endpoint and resolves its /l/?uddg= redirect links back
// to the real target URLs.
use super::html::clean_inline;
use serde_json::{json, Value};
use std::collections::HashSet;

// DDG result blocks: <a class="result__a" href="...">title</a> plus the
// matching <a class="result__snippet">snippet</a> in the same result. The
// same target can surface twice (a wrapped /l/?uddg= link and a direct link,
// or an organic/ad pair), so results are deduped by resolved URL, keeping the
// first occurrence.
pub(crate) fn parse_ddg_results(html: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
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
        let url = resolve_ddg_url(href);
        pos = text_start + rel;
        if seen.insert(url.to_lowercase()) {
            out.push(json!({
                "title": title,
                "url": url,
                "snippet": snippet,
            }));
        }
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

pub(super) fn form_encode(q: &str) -> String {
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

// Shared by the parser tests here and the websearch endpoint test: a real DDG
// results page (two results: one wrapped /l/?uddg= link, one direct link).
#[cfg(test)]
pub(super) const DDG_FIXTURE: &str = r#"<html><body>
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_ddg_results_dedupes_by_resolved_url_keeping_first() {
        // the same target reached both through a wrapped /l/?uddg= link and a
        // direct link, plus a case-differing duplicate: only the first stays.
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2FExample.com%2FDocs&rut=aa">First</a><a class="result__snippet">one</a>
        <a class="result__a" href="https://example.com/docs">Wrapped dup</a><a class="result__snippet">two</a>
        <a class="result__a" href="https://other.example/x">Unique</a><a class="result__snippet">three</a>"#;
        let results = parse_ddg_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["url"], "https://Example.com/Docs");
        assert_eq!(results[0]["title"], "First");
        assert_eq!(results[1]["url"], "https://other.example/x");
    }

    #[test]
    fn percent_and_form_encoding_round_trip_queries() {
        assert_eq!(percent_decode("a%20b%21c"), Some("a b!c".to_string()));
        assert_eq!(percent_decode("%D0%BA"), Some("к".to_string()));
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(form_encode("hello world &к"), "hello+world+%26%D0%BA");
    }
}
