// One typed wrapper per IPC command. JSX must not call invoke() directly.
// Thin barrel: every command lives in exactly one domain module below; all
// existing `from "../api/ipc"` imports and vi.mock("../api/ipc") mocks keep
// working unchanged.
export * from "./ipc/system";
export * from "./ipc/projects";
export * from "./ipc/tasks";
export * from "./ipc/files";
export * from "./ipc/notes";
export * from "./ipc/chat";
export * from "./ipc/harness";
export * from "./ipc/schedules";
export * from "./ipc/settings";
export * from "./ipc/skills";
export * from "./ipc/mcp";
