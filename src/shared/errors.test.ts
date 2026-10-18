import { describe, expect, it } from "vitest";
import { toMessage } from "./errors";

describe("toMessage", () => {
  it("unwraps a non-empty message field, keeps strings, falls back", () => {
    expect(toMessage({ message: "oops" })).toBe("oops");
    expect(toMessage({ message: "" })).toBe("Неизвестная ошибка");
    expect(toMessage("plain")).toBe("plain");
    expect(toMessage(null)).toBe("Неизвестная ошибка");
    expect(toMessage(42)).toBe("Неизвестная ошибка");
  });
});
