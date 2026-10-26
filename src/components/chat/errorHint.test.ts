import { describe, expect, it } from "vitest";
import { EMBED_HINT, LOCAL_HINT, PROVIDER_HINT, errorHint, isEmbeddingError } from "./errorHint";

describe("errorHint", () => {
  it("points an embedder failure at the embedding model, not the LLM", () => {
    const error =
      "Ошибка инструмента: ort coreml pin: CoreMLExecutionProvider execution provider is not supported in this build. Проверьте Настройки — Модель";
    expect(isEmbeddingError(error)).toBe(true);
    expect(errorHint(error)).toBe(EMBED_HINT);
    expect(EMBED_HINT).toContain("модель эмбеддингов");
    expect(EMBED_HINT).toContain("Настройках → Агент");
  });

  it("treats Ort/ONNX/embedding messages as embedding failures", () => {
    expect(errorHint("ort model load: failed to load model_quantized.onnx")).toBe(EMBED_HINT);
    expect(errorHint("embedding backend unavailable")).toBe(EMBED_HINT);
    expect(errorHint("поиск по смыслу: модель не загружена")).toBe(EMBED_HINT);
  });

  it("points plain LLM errors at the provider balance, availability and token limits", () => {
    for (const error of [
      "Провайдер не отвечает 180 секунд",
      "401 Unauthorized: invalid api key",
      "429 Too Many Requests: quota exceeded",
      "Модель не найдена: gpt-x",
      "",
      null,
      undefined,
    ]) {
      expect(isEmbeddingError(error)).toBe(false);
      expect(errorHint(error)).toBe(PROVIDER_HINT);
    }
    expect(PROVIDER_HINT).toContain("баланс");
    expect(PROVIDER_HINT).toContain("доступность API");
    expect(PROVIDER_HINT).toContain("лимиты токенов");
  });

  it("does not blame the provider for explicit local/tool failures", () => {
    expect(errorHint("Ошибка инструмента: path escapes the project root")).toBe(LOCAL_HINT);
    expect(errorHint("database is locked")).toBe(LOCAL_HINT);
    expect(errorHint("permission denied")).toBe(LOCAL_HINT);
  });

  it("treats an opaque error as provider-side", () => {
    expect(errorHint("boom")).toBe(PROVIDER_HINT);
    expect(errorHint("")).toBe(PROVIDER_HINT);
  });
});
