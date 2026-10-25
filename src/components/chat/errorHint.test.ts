import { describe, expect, it } from "vitest";
import { EMBED_HINT, MODEL_HINT, errorHint, isEmbeddingError } from "./errorHint";

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

  it("keeps the model hint for plain LLM errors", () => {
    for (const error of [
      "Провайдер не отвечает 180 секунд",
      "401 Unauthorized: invalid api key",
      "Модель не найдена: gpt-x",
      "",
      null,
      undefined,
    ]) {
      expect(isEmbeddingError(error)).toBe(false);
      expect(errorHint(error)).toBe(MODEL_HINT);
    }
  });
});
