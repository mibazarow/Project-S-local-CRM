// HTML → readable plain text for the network tools: a small state machine
// (script/style/comments dropped, block tags become line breaks, common
// entities decoded, whitespace collapsed), plus the inline-only variant the
// search-result parser uses for titles and snippets.
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

pub(super) fn clean_inline(s: &str) -> String {
    let text = decode_entities(&strip_tags(s));
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
