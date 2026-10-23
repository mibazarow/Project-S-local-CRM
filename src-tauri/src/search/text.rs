// Short window around the first query token found in the stored content, else
// a plain prefix. Used for semantic-only hits and FTS hits whose match was in
// the name column (empty content snippet).
const SNIPPET_WIDTH: usize = 200;

// Splits user input into plain word tokens; FTS operators ("*", quotes, NEAR,
// OR) never survive as syntax, only as word characters. Shared with memory.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

// Each token becomes a quoted phrase (tokens contain no quotes after
// tokenizing), joined with spaces for implicit AND.
pub(crate) fn fts_match_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn fallback_snippet(content: &str, terms_lower: &[String]) -> String {
    let chars: Vec<char> = content.chars().collect();
    let lower = content.to_lowercase();
    let best = terms_lower
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .map(|byte_pos| lower[..byte_pos].chars().count())
        .unwrap_or(0);
    let start = best.saturating_sub(SNIPPET_WIDTH / 3);
    let end = (start + SNIPPET_WIDTH).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}
