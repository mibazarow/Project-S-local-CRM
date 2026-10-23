// Shared HTTP core for the network tools: ureq only, no new deps. Redirects
// are followed manually so the shared SSRF guard (mcp::check_url) re-checks
// every hop before a single packet is sent, and the downloaded body is capped.
// The redirect Location is resolved without a URL crate, and the content-type
// predicates decide whether a body can be handed over as text.
use crate::mcp::check_url;
use std::io::Read;
use std::time::Duration;

// connect fails fast on an unreachable host; read stays generous so a live
// but slow site is not cut off mid-response (ureq read timeout is the gap
// between reads, not the total transfer time).
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FETCH_BYTES: u64 = 2 * 1024 * 1024;
pub(super) const MAX_REDIRECTS: u32 = 5;
// DDG's HTML endpoint answers plain-text bots poorly without a browser UA.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

pub(super) struct Fetched {
    pub(super) final_url: String,
    pub(super) content_type: String,
    pub(super) body: Vec<u8>,
    pub(super) truncated: bool,
}

// GET with manual redirect following: every hop (including Location targets)
// goes through the shared SSRF guard before a single packet is sent.
pub(super) fn fetch(url: &str, connect: Duration, read: Duration) -> std::result::Result<Fetched, String> {
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
pub(super) fn resolve_url(base: &str, loc: &str) -> Option<String> {
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

pub(super) fn is_pass_through(content_type: &str) -> bool {
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

pub(super) fn is_html(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    base == "text/html" || base == "application/xhtml+xml"
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
