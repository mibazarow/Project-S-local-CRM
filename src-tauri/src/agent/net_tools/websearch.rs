// websearch: DuckDuckGo's HTML endpoint (no API key). The query is form
// encoded onto the endpoint URL, the fetched page is parsed by the ddg module
// and the result list is truncated to limit.
use super::ddg::{form_encode, parse_ddg_results};
use super::fetch::{fetch, CONNECT_TIMEOUT, READ_TIMEOUT};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use serde_json::{json, Value};
use std::time::Duration;

const DDG_BASE: &str = "https://html.duckduckgo.com/html/";

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
    #[cfg(test)]
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
    use super::super::ddg::DDG_FIXTURE;
    use super::*;
    use crate::mcp::mock_server::{respond, Handler, MockServer};
    use std::sync::Arc;

    fn mock(handler: Handler) -> MockServer {
        MockServer::start(handler)
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
    #[ignore = "live network: real DuckDuckGo search"]
    fn live_search_finds_results() {
        let r = WebsearchTool::new()
            .execute(&json!({ "query": "tauri framework", "limit": 3 }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(!r.data.unwrap()["results"].as_array().unwrap().is_empty());
    }
}
