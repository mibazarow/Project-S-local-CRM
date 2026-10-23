// Network tools (webfetch + websearch + http_request) for both catalogs.
// ureq only, no new deps: redirects are followed manually so the shared SSRF
// guard (mcp::check_url) re-checks every hop, the download is capped, and HTML
// is reduced to readable text by a small state machine (script/style/comments
// dropped, block tags become line breaks, common entities decoded, whitespace
// collapsed). websearch scrapes DuckDuckGo's HTML endpoint and resolves its
// /l/?uddg= redirect links back to the real target URLs. http_request is the
// general-method twin for API checks: any safe method, custom headers/body,
// textual bodies as UTF-8 text and binary bodies as base64, non-2xx statuses
// returned as data.
// (R-12a) The set is split by concern into submodules: fetch (the shared GET
// core, redirect resolution and content-type predicates), html (HTML to text
// and entities), ddg (search result parsing and the two encoders), webfetch,
// websearch and http_request (the tool impls). This facade re-exports the tool
// structs unchanged, so the external import paths (agent::net_tools::…) and the
// catalog order keep working.
mod ddg;
mod fetch;
mod html;
mod http_request;
mod webfetch;
mod websearch;

pub use http_request::HttpRequestTool;
pub use webfetch::WebfetchTool;
pub use websearch::WebsearchTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::{validate_args, PermissionPolicy, Tool, ToolClass, ToolCtx};
    use serde_json::json;

    #[test]
    fn net_tools_never_shell_out_or_spawn_a_subprocess() {
        // The whole point of Q-18: the internet tools run in-process on ureq,
        // never via a shell or an external HTTP subprocess. Guard the source
        // directly; since the R-12a split that means every submodule too.
        // Tokens are assembled at runtime so this test's own text does not
        // contain the forbidden substrings it is looking for.
        let sources = [
            ("net_tools.rs", include_str!("net_tools.rs")),
            ("net_tools/ddg.rs", include_str!("net_tools/ddg.rs")),
            ("net_tools/fetch.rs", include_str!("net_tools/fetch.rs")),
            ("net_tools/html.rs", include_str!("net_tools/html.rs")),
            ("net_tools/http_request.rs", include_str!("net_tools/http_request.rs")),
            ("net_tools/webfetch.rs", include_str!("net_tools/webfetch.rs")),
            ("net_tools/websearch.rs", include_str!("net_tools/websearch.rs")),
        ];
        let forbidden = [
            ["std", "::", "process"].concat(),
            ["Comm", "and", "::new"].concat(),
            ["cur", "l"].concat(),
        ];
        for (file, src) in sources {
            for token in &forbidden {
                assert!(!src.contains(token), "{file} must not contain {token:?}");
            }
        }
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
}
