import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import ConfirmDialogHost from "./ConfirmDialogHost";
import { confirmAction, confirmRemove } from "../../shared/confirm";
import type { ConfirmOptions } from "../../shared/confirm";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => root.render(<ConfirmDialogHost />));
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const dialog = () => document.body.querySelector(".confirm-dialog");
const dialogBtn = (label: string) =>
  Array.from(dialog()!.querySelectorAll<HTMLButtonElement>("button")).find(
    (b) => b.textContent?.trim() === label,
  )!;

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

function ask(opts?: Partial<ConfirmOptions>) {
  let result: boolean | undefined;
  act(() => {
    confirmAction({
      title: "Удалить файл «notes.md»?",
      message: "Файл будет перемещён в корзину.",
      ...opts,
    }).then((ok) => (result = ok));
  });
  return () => result;
}

describe("ConfirmDialogHost", () => {
  it("renders title, message and labels and resolves true on confirm click", async () => {
    const result = ask({ confirmLabel: "Удалить", danger: true });
    expect(dialog()).not.toBeNull();
    expect(dialog()!.textContent).toContain("Удалить файл «notes.md»?");
    expect(dialog()!.textContent).toContain("Файл будет перемещён в корзину.");
    expect(dialog()!.getAttribute("role")).toBe("alertdialog");
    await click(dialogBtn("Удалить"));
    expect(result()).toBe(true);
    expect(dialog()).toBeNull();
  });

  it("marks the destructive confirm button with the danger class", async () => {
    ask({ confirmLabel: "Удалить", danger: true });
    expect(dialogBtn("Удалить").className).toBe("primary-btn danger");
    ask({ confirmLabel: "ОК" });
    expect(dialogBtn("ОК").className).toBe("primary-btn");
  });

  it("resolves false on the cancel button", async () => {
    const result = ask({ cancelLabel: "Не удалять" });
    await click(dialogBtn("Не удалять"));
    expect(result()).toBe(false);
    expect(dialog()).toBeNull();
  });

  it("resolves false on Escape", async () => {
    const result = ask();
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    });
    expect(result()).toBe(false);
    expect(dialog()).toBeNull();
  });

  it("resolves false on backdrop click but not on panel click", async () => {
    const result = ask();
    await click(dialog()!);
    expect(result()).toBeUndefined();
    await click(document.body.querySelector(".confirm-backdrop")!);
    expect(result()).toBe(false);
  });

  it("confirmRemove defaults to a red «Удалить» button and «Отмена»", async () => {
    let result: boolean | undefined;
    act(() => {
      confirmRemove({ title: "Удалить папку «src»?" }).then((ok) => (result = ok));
    });
    expect(dialogBtn("Удалить").className).toBe("primary-btn danger");
    expect(dialogBtn("Отмена")).toBeDefined();
    await click(dialogBtn("Удалить"));
    expect(result).toBe(true);
  });

  it("resolves false when the host unmounts while the dialog is open", async () => {
    const result = ask();
    await act(async () => root.render(null));
    expect(result()).toBe(false);
  });
});
