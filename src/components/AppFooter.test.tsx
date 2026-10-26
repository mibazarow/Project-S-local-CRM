import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";
import AppFooter from "./AppFooter";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("AppFooter", () => {
  it("renders the developer and support links", () => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    act(() => root.render(<AppFooter />));

    const links = [...container.querySelectorAll("a")];
    expect(links.map((a) => a.getAttribute("href"))).toEqual([
      "https://bazarow.ru/",
      "https://www.donationalerts.com/r/bazarow_ru",
    ]);
    expect(container.textContent).toContain("Разработка:");
    expect(container.textContent).toContain("https://bazarow.ru");
    expect(container.textContent).toContain("Поддержать проект");
  });
});
