// The footer under a chat error must tell the user where to look. Embedder
// failures are local; everything else that prevents an answer is most likely
// provider-side (balance, API availability, token limits), so the default hint
// points there instead of showing an opaque backend error alone.

export const EMBED_HINT =
  "Поиск по смыслу недоступен: не удалось загрузить модель эмбеддингов. " +
  "Ответы и поиск по словам продолжают работать. Проверьте модель в Настройках → Агент.";

export const PROVIDER_HINT =
  "Похоже, проблема на стороне API-провайдера. Проверьте баланс и оплату, " +
  "доступность API (endpoint и ключ) и лимиты токенов. Если всё в порядке — повторите запрос.";

export const LOCAL_HINT =
  "Ошибка приложения или инструмента — подробности выше. Если она повторяется, пришлите лог.";

// Raw backend messages for an embedder load/run failure: "ort coreml pin: ...",
// "ort model load: ...", "tokenize: ...", "embedder: ...", "поиск по смыслу".
const EMBED_MARKERS = ["coreml", "onnx", "embedder", "embedding", "эмбеддинг", "ort ", "поиск по смыслу"];

export function isEmbeddingError(error: string | null | undefined): boolean {
  if (!error) return false;
  const text = error.toLowerCase();
  return EMBED_MARKERS.some((marker) => text.includes(marker));
}

const PROVIDER_MARKERS = [
  "provider", "провайдер", "api", "401", "403", "429", "500", "502", "503", "504",
  "unauthorized", "forbidden", "quota", "rate limit", "balance", "insufficient",
  "token", "model not found", "модель не найдена", "timeout", "timed out",
  "не отвечает", "connection", "network", "dns", "tls", "endpoint", "stream", "llm",
];
const LOCAL_MARKERS = [
  "ошибка инструмента", "path", "escapes", "permission", "access denied",
  "database", "sqlite", "invalid", "cancelled", "canceled", "stopped by user",
  "no such file", "os error",
];

// Unknown failures are treated as provider-side: that is the actionable default
// for an opaque chat error, while explicit local/tool failures stay local.
export function errorHint(error: string | null | undefined): string {
  if (isEmbeddingError(error)) return EMBED_HINT;
  const text = (error ?? "").toLowerCase();
  if (PROVIDER_MARKERS.some((marker) => text.includes(marker))) return PROVIDER_HINT;
  if (LOCAL_MARKERS.some((marker) => text.includes(marker))) return LOCAL_HINT;
  return PROVIDER_HINT;
}
