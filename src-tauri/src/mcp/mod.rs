// MCP client core (SPEC 6.2): connect configured servers over a pluggable
// transport, run the initialize handshake, discover tools/list, and bridge
// them into the agent catalog as mcp_<server>__<tool>. Everything a server
// sends (descriptions, results) is untrusted data, never instructions;
// configured secrets are masked in every message leaving this module.
//
// Layout: `client` (handshake + tools/list + tools/call), `lifecycle`
// (mcp.json → registry → status/shutdown), `tools` (catalog bridge and
// schema flattening), `guard` (SSRF denylist + secret masking), and the
// transports `stdio` / `http` / `sse` over the `jsonrpc` framing seam.
mod client;
mod guard;
mod lifecycle;
mod tools;

pub mod http;
pub mod jsonrpc;
#[cfg(test)]
pub(crate) mod mock_server;
pub mod sse;
pub mod stdio;

pub(crate) use guard::check_url;
pub use lifecycle::{McpRegistry, McpServerStatus};
