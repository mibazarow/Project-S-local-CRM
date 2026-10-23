// MCP server lifecycle (SPEC 6.2): read mcp.json, start every enabled
// server over its transport, keep one status row per server, bridge its
// tools, and shut the clients down. A server that fails to start is an
// error row, never a broken app.
use super::client::{McpClient, McpTool, CALL_TIMEOUT};
use super::guard::{check_url, mask_secrets};
use super::jsonrpc::McpTransport;
use super::tools::McpToolBridge;
use super::{http, sse, stdio};
use crate::agent::tool::Tool;
use crate::config::{self, ConfigKind, McpServerConfig, McpTransportConfig};
use crate::error::AppError;
use crate::paths;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub enabled: bool,
    pub transport: &'static str,
    pub status: &'static str,
    pub error: Option<String>,
    pub tool_count: usize,
}

struct ServerHandle {
    name: String,
    enabled: bool,
    transport: &'static str,
    error: Option<String>,
    client: Option<Arc<Mutex<McpClient>>>,
    tools: Vec<McpTool>,
    secrets: Vec<String>,
}

#[derive(Default)]
pub struct McpRegistry {
    servers: Vec<ServerHandle>,
}

impl McpRegistry {
    // Reads mcp.json and starts every enabled server at startup; a broken
    // config file becomes one error row instead of breaking the app.
    pub fn start(wd: &Path) -> McpRegistry {
        let value = match config::read_config(wd, ConfigKind::Mcp) {
            Ok(value) => value,
            Err(e) => {
                return McpRegistry {
                    servers: vec![config_error_row("mcp.json", format!("config error: {e}"))],
                }
            }
        };
        let servers = value
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        McpRegistry::start_from(&servers, wd)
    }

    pub(super) fn start_from(servers: &[Value], wd: &Path) -> McpRegistry {
        McpRegistry {
            servers: servers.iter().map(|entry| start_server(entry, wd)).collect(),
        }
    }

    // Bridged catalog tools for every running server (SPEC 6.2 naming).
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        for server in &self.servers {
            let Some(client) = &server.client else {
                continue;
            };
            let secrets = Arc::new(server.secrets.clone());
            for tool in &server.tools {
                out.push(Arc::new(McpToolBridge::new(
                    &server.name,
                    tool,
                    Arc::clone(client),
                    Arc::clone(&secrets),
                )) as Arc<dyn Tool>);
            }
        }
        out
    }

    pub fn status(&self) -> Vec<McpServerStatus> {
        self.servers
            .iter()
            .map(|server| McpServerStatus {
                name: server.name.clone(),
                enabled: server.enabled,
                transport: server.transport,
                status: if server.error.is_some() {
                    "error"
                } else if !server.enabled {
                    "disabled"
                } else {
                    "running"
                },
                error: server.error.clone(),
                tool_count: server.tools.len(),
            })
            .collect()
    }

    // Closes every client; the stdio transports kill their children on drop.
    pub fn shutdown(&mut self) {
        for server in &mut self.servers {
            if let Some(client) = server.client.take() {
                if let Ok(mut client) = client.lock() {
                    client.close();
                }
            }
        }
        self.servers.clear();
    }
}

fn config_error_row(name: &str, error: String) -> ServerHandle {
    ServerHandle {
        name: name.to_string(),
        enabled: false,
        transport: "unknown",
        error: Some(error),
        client: None,
        tools: Vec::new(),
        secrets: Vec::new(),
    }
}

fn transport_label(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Stdio { .. } => "stdio",
        McpTransportConfig::Http { .. } => "http",
        McpTransportConfig::Sse { .. } => "sse",
    }
}

// One server entry from mcp.json: validate, connect (spawn for stdio,
// streamable HTTP or legacy SSE for remote), handshake, list tools. Any
// failure is recorded on the row; the tools of a failed server are simply
// skipped (SPEC 6.2 "don't break the app").
fn start_server(entry: &Value, wd: &Path) -> ServerHandle {
    let config = match serde_json::from_value::<McpServerConfig>(entry.clone()) {
        Ok(config) => config,
        Err(e) => {
            return ServerHandle {
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unnamed>")
                    .to_string(),
                enabled: entry.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                transport: "unknown",
                error: Some(format!("invalid server config: {e}")),
                client: None,
                tools: Vec::new(),
                secrets: Vec::new(),
            }
        }
    };
    let transport_kind = transport_label(&config.transport);
    let call_timeout = config
        .timeout_secs
        .map(|secs| Duration::from_secs(secs))
        .unwrap_or(CALL_TIMEOUT);
    if !config.enabled {
        return ServerHandle {
            name: config.name,
            enabled: false,
            transport: transport_kind,
            error: None,
            client: None,
            tools: Vec::new(),
            secrets: Vec::new(),
        };
    }
    let (transport, secrets) = match &config.transport {
        McpTransportConfig::Stdio { command, args, env } => (
            stdio::StdioTransport::spawn(command, args, env, &paths::harness_tmp_dir(wd))
                .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
                .map_err(|e| AppError::Io(format!("failed to spawn {command}: {e}"))),
            env.values().cloned().collect(),
        ),
        McpTransportConfig::Http { url, headers } => (
            check_url(url).and_then(|_| {
                http::HttpTransport::new(url, headers, call_timeout)
                    .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
            }),
            headers.values().cloned().collect(),
        ),
        McpTransportConfig::Sse { url, headers } => (
            check_url(url).and_then(|_| {
                sse::SseTransport::connect(url, headers)
                    .map(|transport| Box::new(transport) as Box<dyn McpTransport>)
            }),
            headers.values().cloned().collect(),
        ),
    };
    let outcome = transport.and_then(|transport| {
        let mut client = McpClient::connect_with(transport, call_timeout)?;
        let tools = client.list_tools()?;
        Ok((client, tools))
    });
    match outcome {
        Ok((client, tools)) => ServerHandle {
            name: config.name,
            enabled: true,
            transport: transport_kind,
            error: None,
            client: Some(Arc::new(Mutex::new(client))),
            tools,
            secrets,
        },
        Err(e) => ServerHandle {
            name: config.name,
            enabled: true,
            transport: transport_kind,
            error: Some(mask_secrets(&e.to_string(), &secrets)),
            client: None,
            tools: Vec::new(),
            secrets,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::ToolCtx;
    use crate::mcp::mock_server;
    use crate::onboarding::temp_test_dir;
    use serde_json::json;
    use std::fs;

    #[test]
    fn failing_server_is_skipped_with_status_error_and_no_panic() {
        let wd = temp_test_dir("mcp-bad-server");
        let registry = McpRegistry::start_from(
            &[json!({ "name": "broken", "enabled": true,
                      "transport": { "type": "stdio", "command": "this-command-does-not-exist-xyz", "args": [] } })],
            &wd,
        );
        let status = registry.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status, "error");
        assert!(status[0].error.as_ref().unwrap().contains("this-command-does-not-exist-xyz"));
        assert!(registry.tools().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn disabled_server_reports_disabled_and_starts_nothing() {
        let registry = McpRegistry::start_from(
            &[json!({ "name": "off", "enabled": false, "transport": { "type": "stdio", "command": "nope" } })],
            Path::new("/nonexistent-wd"),
        );
        assert_eq!(registry.status()[0].status, "disabled");
        assert!(registry.tools().is_empty());
    }

    #[test]
    fn stdio_server_honors_per_server_timeout_secs() {
        let wd = temp_test_dir("mcp-timeout-secs");
        // Answers initialize, then goes silent: tools/list must time out
        // after the configured 1s, not the default 120s.
        let registry = McpRegistry::start_from(
            &[json!({ "name": "slow", "enabled": true, "timeoutSecs": 1,
                      "transport": { "type": "stdio", "command": "sh", "args": [
                          "-c", "read request; printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; sleep 10"
                      ] } })],
            &wd,
        );
        let status = registry.status();
        assert_eq!(status[0].status, "error");
        assert!(status[0].error.as_ref().unwrap().contains("did not answer within 1s"),
                "{:?}", status[0].error);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn registry_shutdown_kills_the_stdio_child_process() {
        let wd = temp_test_dir("mcp-shutdown-kill");
        // The child writes its own pid to a file in the wd (spawn points the
        // cwd at harness_tmp under the wd? no — it inherits ours, so use an
        // absolute path), answers initialize, then loops forever; after
        // shutdown the pid must be gone.
        let pid_file = wd.join("child.pid");
        let script = format!(
            "read request; echo $$ > {}; printf '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}\\n'; while read _; do printf '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{}}}}\\n'; done",
            pid_file.display()
        );
        let registry = McpRegistry::start_from(
            &[json!({ "name": "loop", "enabled": true,
                      "transport": { "type": "stdio", "command": "sh", "args": ["-c", script] } })],
            &wd,
        );
        assert_eq!(registry.status()[0].status, "running");
        let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
        assert!(!pid.is_empty());
        let mut registry = registry;
        registry.shutdown();
        let mut gone = false;
        for _ in 0..40 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid])
                .status()
                .map_or(false, |s| s.success());
            if !alive {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone, "stdio child {pid} must be killed by shutdown");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn remote_urls_are_validated_against_denylist_scheme_and_connectivity() {
        let registry = McpRegistry::start_from(
            &[
                json!({ "name": "meta", "enabled": true, "transport": { "type": "http", "url": "http://169.254.169.254/mcp" } }),
                json!({ "name": "refused", "enabled": true, "transport": { "type": "sse", "url": "http://127.0.0.1:1/sse", "headers": { "Authorization": "Bearer s3cret-token" } } }),
                json!({ "name": "bad-scheme", "enabled": true, "transport": { "type": "http", "url": "ftp://mcp.example.ru/mcp" } }),
            ],
            Path::new("/nonexistent-wd"),
        );
        let status = registry.status();
        assert_eq!(status[0].status, "error");
        assert_eq!(status[0].transport, "http");
        assert!(status[0].error.as_ref().unwrap().contains("ssrf"));
        assert_eq!(status[1].status, "error");
        assert_eq!(status[1].transport, "sse");
        assert!(status[1].error.as_ref().unwrap().contains("connection error"));
        assert!(!status[1].error.as_ref().unwrap().contains("s3cret-token"));
        assert_eq!(status[2].status, "error");
        assert!(status[2].error.as_ref().unwrap().contains("not a plain http(s) url"));
    }

    #[test]
    fn http_server_tools_are_bridged_and_status_reports_transport() {
        let wd = temp_test_dir("mcp-http-bridge");
        let handler: mock_server::Handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &mock_server::CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                if body["method"].as_str() == Some("notifications/initialized") {
                    return mock_server::respond(stream, 202, "application/json", "", &[]);
                }
                let answer = mock_server::mcp_answer(body["method"].as_str().unwrap_or(""), &body["id"]);
                mock_server::respond(stream, 200, "application/json", &answer.to_string(), &[("Mcp-Session-Id", "sess-7")])
            },
        );
        let server = mock_server::MockServer::start(handler);
        let registry = McpRegistry::start_from(
            &[json!({ "name": "Remote", "enabled": true,
                      "transport": { "type": "http", "url": format!("{}/mcp", server.url_base) } })],
            &wd,
        );
        let status = registry.status();
        assert_eq!(status[0].status, "running");
        assert_eq!(status[0].transport, "http");
        assert_eq!(status[0].tool_count, 1);
        let tools = registry.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp_remote__echo");
        let result = tools[0]
            .execute(&json!({ "text": "hi" }), &ToolCtx::default())
            .unwrap();
        assert!(result.ok);
        assert!(result.data.unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("echo: hi"));
        let log = server.request_log();
        assert!(log.iter().any(|r| r.headers.get("mcp-session-id").map(String::as_str) == Some("sess-7")));
        fs::remove_dir_all(&wd).unwrap();
    }
}
