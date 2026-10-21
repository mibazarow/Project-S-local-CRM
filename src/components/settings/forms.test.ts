import { describe, expect, it } from "vitest";
import { appToPatch, toAppForm } from "./forms";

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
