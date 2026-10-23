// The MCP client side of the protocol (SPEC 6.2): the initialize handshake
// over any transport, tools/list discovery and tools/call. Everything a
// server sends back is untrusted data, never instructions.
use super::jsonrpc::{McpTransport, RpcChannel};
use crate::error::Result;
use serde_json::{json, Value};
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(120);

pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpCallOutcome {
    pub text: String,
    pub is_error: bool,
}

pub struct McpClient {
    channel: RpcChannel,
    call_timeout: Duration,
}

impl McpClient {
    // initialize handshake + initialized notification over any transport.
    pub fn connect_with(transport: Box<dyn McpTransport>, call_timeout: Duration) -> Result<McpClient> {
        let mut channel = RpcChannel::new(transport);
        channel.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "project-s", "version": env!("CARGO_PKG_VERSION") },
            }),
            CONNECT_TIMEOUT,
        )?;
        channel.notify("notifications/initialized", json!({}))?;
        Ok(McpClient {
            channel,
            call_timeout,
        })
    }

    pub fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let result = self
            .channel
            .request("tools/list", json!({}), self.call_timeout)?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .iter()
            .filter_map(|entry| {
                Some(McpTool {
                    name: entry.get("name")?.as_str()?.to_string(),
                    description: entry
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: entry
                        .get("inputSchema")
                        .cloned()
                        .filter(|schema| schema.is_object())
                        .unwrap_or_else(|| json!({ "type": "object" })),
                })
            })
            .collect())
    }

    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<McpCallOutcome> {
        let result = self.channel.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
            self.call_timeout,
        )?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<&str>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(McpCallOutcome {
            text,
            is_error: result.get("isError").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn close(&mut self) {
        self.channel.close();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mcp::jsonrpc::MockTransport;
    use serde_json::json;

    pub(crate) fn init_ok() -> Value {
        json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                        "serverInfo": { "name": "test", "version": "0" } }
        })
    }

    #[test]
    fn list_tools_parses_names_descriptions_and_schemas() {
        let mut client = McpClient::connect_with(Box::new(MockTransport::new(vec![
            init_ok(),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [
                { "name": "get_thing", "description": "Gets a thing",
                  "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] } },
                { "name": "no_schema" },
            ] } }),
        ])), CALL_TIMEOUT)
        .unwrap();
        let tools = client.list_tools().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "get_thing");
        assert_eq!(tools[0].description, "Gets a thing");
        assert_eq!(tools[0].input_schema["properties"]["id"]["type"], "string");
        assert_eq!(tools[1].input_schema, json!({ "type": "object" }));
    }
}
