// Safety guards shared by the MCP servers and the agent's network tools:
// the SSRF denylist (SPEC 6.2) and secret masking for anything that leaves
// a server boundary.
use crate::error::{AppError, Result};

// SSRF denylist (SPEC 6.2): cloud metadata endpoints and link-local ranges
// are refused for http/sse servers; localhost, LAN and public hosts stay
// allowed. Kept as a pure function so it is directly testable.
pub fn ssrf_denied_host(host: &str) -> bool {
    let host = host.trim().to_lowercase();
    let host = host.trim_end_matches('.');
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if matches!(
        bare,
        "169.254.169.254" | "100.100.100.200" | "metadata.google.internal" | "metadata"
    ) {
        return true;
    }
    if let Some(octets) = parse_ipv4(bare) {
        // link-local IPv4 (169.254.0.0/16) covers the common metadata IPs.
        return octets[0] == 169 && octets[1] == 254;
    }
    bare.starts_with("fe80")
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        octets[i] = part.parse().ok()?;
    }
    Some(octets)
}

// Host of an http(s) URL without a URL crate: scheme and credentials
// stripped, port cut, bracketed IPv6 handled.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if let Some(bracketed) = host.strip_prefix('[') {
        return bracketed.split_once(']').map(|(host, _)| host.to_string());
    }
    match host.rsplit_once(':') {
        Some((host, _)) => Some(host.to_string()),
        None => (!host.is_empty()).then(|| host.to_string()),
    }
}

// Remote servers must be plain http(s) URLs whose host passes the SSRF
// denylist; anything else is refused before a single packet is sent.
// Shared with the agent's network tools (agent/net_tools.rs) so the
// denylist lives in exactly one place.
pub(crate) fn check_url(url: &str) -> Result<()> {
    let scheme_ok = url.starts_with("http://") || url.starts_with("https://");
    match url_host(url) {
        Some(host) if scheme_ok && !ssrf_denied_host(&host) => Ok(()),
        _ => Err(AppError::Invalid(format!(
            "refused: {url} is not a plain http(s) url or its host is on the ssrf deny list"
        ))),
    }
}

// Masks configured secrets (stdio env values, http header values) in
// anything that leaves the server boundary: status errors and tool errors.
pub fn mask_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= 4 {
            out = out.replace(secret, "<redacted>");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_denylist_blocks_metadata_endpoints_and_allows_local_lan_public() {
        for blocked in [
            "169.254.169.254",
            "metadata.google.internal",
            "metadata",
            "169.254.1.1",
            "100.100.100.200",
            "fe80::1",
        ] {
            assert!(ssrf_denied_host(blocked), "{blocked} must be blocked");
        }
        for allowed in [
            "localhost",
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.10",
            "8.8.8.8",
            "mcp.example.ru",
        ] {
            assert!(!ssrf_denied_host(allowed), "{allowed} must be allowed");
        }
    }

    #[test]
    fn url_host_strips_scheme_credentials_and_port() {
        assert_eq!(url_host("http://127.0.0.1:8080/mcp").as_deref(), Some("127.0.0.1"));
        assert_eq!(url_host("https://user:pw@mcp.example.ru/sse").as_deref(), Some("mcp.example.ru"));
        assert_eq!(url_host("http://[fe80::1]:9000").as_deref(), Some("fe80::1"));
        assert_eq!(url_host("not a url"), None);
    }
}
