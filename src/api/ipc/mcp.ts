// MCP IPC (SPEC 6.2): per-server status and reload; add/edit/delete go through
// read_config("mcp")/write_config("mcp") then reload; the enabled toggle is
// the dedicated command (persists mcp.json, restarts the server, emits
// config-changed).
import { invoke } from "@tauri-apps/api/core";
import type { McpServerStatus } from "../types";

export const mcpStatus = () => invoke<McpServerStatus[]>("mcp_status");

export const mcpReload = () => invoke<McpServerStatus[]>("mcp_reload");

export const mcpSetEnabled = (name: string, enabled: boolean) =>
  invoke<McpServerStatus[]>("mcp_set_enabled", { name, enabled });
