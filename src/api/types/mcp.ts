// mcp::McpServerStatus — one row of mcp_status / mcp_reload.

export interface McpServerStatus {
  name: string;
  enabled: boolean;
  // "stdio" | "http" | "sse" | "unknown" (broken config row).
  transport: string;
  status: "running" | "error" | "disabled";
  error: string | null;
  toolCount: number;
}
