export type ProviderPreset = "deepseek" | "openai" | "ollama";

export const PROVIDER_PRESETS: Record<ProviderPreset, { name: string; baseUrl: string; model?: string }> = {
  deepseek: {
    name: "DeepSeek",
    baseUrl: "https://api.deepseek.com/v1",
    model: "deepseek-chat",
  },
  openai: {
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
  },
  ollama: {
    name: "Ollama",
    baseUrl: "http://localhost:11434/v1",
  },
};

// Endpoint host shown under a provider name (scheme stripped, path dropped).
export function endpointHost(url: string): string {
  const after = url.trim().split("://")[1] ?? url.trim();
  return after.split("/")[0];
}

export function isLocalEndpoint(url: string): boolean {
  return /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])([:/]|$)/i.test(url.trim());
}

export function validateProviderInput(name: string, baseUrl: string, apiKey: string): string | null {
  if (!name) return "Укажи имя провайдера";
  if (!/^https?:\/\//i.test(baseUrl)) {
    return "Endpoint должен начинаться с http:// или https://";
  }
  if (!apiKey && !isLocalEndpoint(baseUrl)) {
    return "Укажи API-ключ (пусто разрешено только для localhost)";
  }
  return null;
}
