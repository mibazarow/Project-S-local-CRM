import { describe, expect, it } from "vitest";
import { appToPatch, llmToPatch, toAppForm, toLlmForm } from "./forms";

describe("app form ui scale (Q.md item 4)", () => {
  it("defaults the scale to 100 when the config has no value", () => {
    expect(toAppForm({}).uiScalePercent).toBe(100);
  });

  it("normalizes an invalid saved scale to 100", () => {
    expect(toAppForm({ uiScalePercent: 137 }).uiScalePercent).toBe(100);
    expect(toAppForm({ uiScalePercent: 0 }).uiScalePercent).toBe(100);
  });

  it("keeps a valid step", () => {
    expect(toAppForm({ uiScalePercent: 175 }).uiScalePercent).toBe(175);
  });

  it("drops a removed step to 100", () => {
    expect(toAppForm({ uiScalePercent: 125 }).uiScalePercent).toBe(100);
    expect(toAppForm({ uiScalePercent: 150 }).uiScalePercent).toBe(100);
    expect(toAppForm({ uiScalePercent: 200 }).uiScalePercent).toBe(100);
  });

  it("writes a normalized uiScalePercent and preserves other raw keys", () => {
    const form = toAppForm({ uiScalePercent: 175 });
    const patch = appToPatch(form, { uiScalePercent: 137, futureKey: 7 });
    expect(patch.uiScalePercent).toBe(175);
    expect(patch.futureKey).toBe(7);
  });

  it("clamps an out-of-range form value back to 100 on write", () => {
    const patch = appToPatch({ ...toAppForm({}), uiScalePercent: 999 }, {});
    expect(patch.uiScalePercent).toBe(100);
  });
});

describe("llm per-model context windows", () => {
  it("reads the selected model's window from modelContextWindows", () => {
    const form = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 240000,
      temperature: null,
      maxTokens: null,
      modelContextWindows: { m: 128000, other: 50000 },
      modelContextMax: { m: 200000 },
    });
    expect(form.contextWindow).toBe("128000");
    expect(form.modelContextMax.m).toBe(200000);
  });

  it("defaults to 100k when the model has no entry and no reported max", () => {
    const form = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 65536,
      temperature: null,
      maxTokens: null,
    });
    expect(form.contextWindow).toBe("100000");
    const legacy = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 240000,
      temperature: null,
      maxTokens: null,
    });
    expect(legacy.contextWindow).toBe("100000");
  });

  it("uses the reported maximum when the model has no saved window", () => {
    const form = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 240000,
      temperature: null,
      maxTokens: null,
      modelContextMax: { m: 64000 },
    });
    expect(form.contextWindow).toBe("64000");
  });

  it("clamps a saved window above the provider maximum on write", () => {
    const form = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 100000,
      temperature: null,
      maxTokens: null,
      modelContextWindows: { m: 300000 },
      modelContextMax: { m: 128000 },
    });
    expect(form.contextWindow).toBe("128000");
    const patch = llmToPatch({ ...form, contextWindow: "400000" }, { futureKey: 7 });
    expect((patch.modelContextWindows as Record<string, number>).m).toBe(128000);
    expect(patch.contextWindow).toBe(128000);
    expect(patch.futureKey).toBe(7);
  });

  it("caps an unreported model at 250k on write", () => {
    const form = toLlmForm({
      baseUrl: "b",
      apiKey: "",
      model: "m",
      contextWindow: 100000,
      temperature: null,
      maxTokens: null,
    });
    const patch = llmToPatch({ ...form, contextWindow: "400000" }, {});
    expect((patch.modelContextWindows as Record<string, number>).m).toBe(250000);
  });
});
