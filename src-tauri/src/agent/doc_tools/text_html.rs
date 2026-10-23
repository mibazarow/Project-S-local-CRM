// Plain text / HTML writers: md and txt keep the Markdown as-is (with an
// optional title prefix), html wraps it in a minimal escaped page.
pub(super) fn plain_with_title(title: Option<&str>, markdown: &str, as_markdown: bool) -> String {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    match title {
        Some(t) if as_markdown => format!("# {t}\n\n{markdown}"),
        Some(t) => format!("{t}\n\n{markdown}"),
        None => markdown.to_string(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub(super) fn html_wrapper(title: Option<&str>, markdown: &str) -> String {
    let t = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Document");
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n</head>\n<body>\n<h1>{}</h1>\n<pre>{}</pre>\n</body>\n</html>\n",
        html_escape(t),
        html_escape(t),
        html_escape(markdown)
    )
}
