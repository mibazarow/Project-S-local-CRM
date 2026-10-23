// LLM config and provider-profile DTOs (config.rs LlmConfig, llm::*).

// Shape of llm.json (config.rs LlmConfig).
export interface LlmConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
  contextWindow: number;
  // Per-model context windows chosen in Settings; `contextWindow` above is
  // the fallback for models without an entry.
  modelContextWindows?: Record<string, number>;
  // Provider-reported maximum context window per model id.
  modelContextMax?: Record<string, number>;
  temperature: number | null;
  maxTokens: number | null;
  // Manual vision override: null/absent = auto-detect, true/false = force.
  vision?: boolean | null;
  // Cached result of the last vision probe (written by detect_vision).
  visionDetected?: boolean | null;
  // Thinking mode level: "off" omits reasoning_effort, the rest send it as-is.
  thinkingLevel?: "off" | "low" | "medium" | "high";
  // Saved provider profiles (Q-24): any number, exactly one active. The
  // top-level baseUrl/apiKey mirror the active provider (legacy fallback).
  providers?: LlmProvider[];
  activeProvider?: string | null;
  lastModelByProvider?: Record<string, string>;
}

// A saved provider profile (config.rs LlmProvider). baseUrl/apiKey are the
// endpoint and key; name is the user label.
export interface LlmProvider {
  id: string;
  name: string;
  baseUrl: string;
  apiKey: string;
}

// llm::LlmProviderDto — a provider row returned by the provider commands.
export interface LlmProviderDto extends LlmProvider {
  active: boolean;
}

// llm::VisionStatus — result of get_vision_status / detect_vision.
export interface VisionStatus {
  // null = unknown (never probed, or the probe was inconclusive).
  supported: boolean | null;
  // True when llm.json `vision` manually overrides detection.
  overridden: boolean;
  reason: string | null;
}

export interface ModelInfo {
  id: string;
  // Maximum context window reported by the provider; null when unknown.
  maxContext: number | null;
}

// Result of test_connection (llm::ConnectionTest).
export interface ConnectionTest {
  ok: boolean;
  message: string;
  modelCount: number;
}
