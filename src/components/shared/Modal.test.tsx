import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import Modal from "./Modal";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function render(el: React.ReactElement) {
  await act(async () => {
    root.render(el);
  });
}

async function click(el: Element) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

async function pressEsc() {
  await act(async () => {
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  });
}

const backdrop = () => container.querySelector<HTMLElement>(".modal-backdrop")!;
const panel = () => container.querySelector<HTMLElement>(".modal")!;

describe("Modal shell", () => {
  it("renders backdrop and panel classes unchanged", async () => {
    await render(
      <Modal onClose={() => {}} className="modal version-modal">
        <h3>Title</h3>
      </Modal>,
    );
    expect(backdrop().querySelector(":scope > .modal.version-modal")).not.toBeNull();
  });

  it("closes on backdrop click by default", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await click(backdrop());
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not close on backdrop click when closeOnBackdrop is false", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} closeOnBackdrop={false} className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await click(backdrop());
    expect(onClose).not.toHaveBeenCalled();
  });

  it("does not close on Escape by default", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await pressEsc();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes on Escape only when closeOnEsc is set", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} closeOnEsc className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await pressEsc();
    expect(onClose).toHaveBeenCalledTimes(1);
    await pressEsc();
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("ignores other keys when closeOnEsc is set", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} closeOnEsc className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
    });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("does not close on content clicks", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} className="modal">
        <h3>Title</h3>
      </Modal>,
    );
    await click(panel());
    await click(panel().querySelector("h3")!);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes via a close button rendered in children", async () => {
    const onClose = vi.fn();
    await render(
      <Modal onClose={onClose} className="modal">
        <button className="icon-btn small" title="Закрыть" onClick={onClose}>
          x
        </button>
      </Modal>,
    );
    await click(panel().querySelector("button")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("supports a custom backdrop class", async () => {
    await render(
      <Modal onClose={() => {}} backdropClassName="palette-backdrop" className="palette">
        <h3>Title</h3>
      </Modal>,
    );
    expect(container.querySelector(".palette-backdrop > .palette")).not.toBeNull();
  });
});
