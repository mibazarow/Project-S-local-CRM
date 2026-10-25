// The footers under a chat error used to be one generic "check the model in
// Settings" line, which pointed LLM users at the wrong setting when the real
// failure came from the on-device embedding model (ONNX Runtime / CoreML) used
// for semantic search. The embedder is not an LLM, and word search keeps
// working without it, so it gets its own hint.

export const MODEL_HINT = "Проверьте Настройки → Модель.";

export const EMBED_HINT =
  "Поиск по смыслу недоступен: не удалось загрузить модель эмбеддингов. " +
  "Ответы и поиск по словам продолжают работать. Проверьте модель в Настройках → Агент.";

// Raw backend messages for an embedder load/run failure: "ort coreml pin: ...",
// "ort model load: ...", "tokenize: ...", "embedder: ...", "поиск по смыслу".
const EMBED_MARKERS = ["coreml", "onnx", "embedder", "embedding", "эмбеддинг", "ort ", "поиск по смыслу"];

export function isEmbeddingError(error: string | null | undefined): boolean {
  if (!error) return false;
  const text = error.toLowerCase();
  return EMBED_MARKERS.some((marker) => text.includes(marker));
}

export function errorHint(error: string | null | undefined): string {
  return isEmbeddingError(error) ? EMBED_HINT : MODEL_HINT;
}
