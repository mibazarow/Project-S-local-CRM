import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import Switch from "../shared/Switch";
import { Field } from "./shared";

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

const fieldLabel = () => container.querySelector<HTMLLabelElement>(".set-field-label")!;
const controlBox = () => container.querySelector(".set-field-control")!;

describe("Field label association", () => {
  it("keeps the layout classes and the control inside the control box", async () => {
    await render(
      <Field label="Имя" hint="Подсказка" error="Ошибка">
        <input type="text" />
      </Field>,
    );
    expect(container.querySelector(".set-field")!.className).toBe("set-field");
    expect(container.querySelector(".set-field-info")).not.toBeNull();
    expect(fieldLabel().textContent).toBe("Имя");
    expect(controlBox().querySelector("input")).not.toBeNull();
  });

  it("links the label to a plain input with a stable id", async () => {
    await render(
      <Field label="Имя">
        <input type="text" defaultValue="x" />
      </Field>,
    );
    const input = controlBox().querySelector("input")!;
    expect(input.id).not.toBe("");
    expect(fieldLabel().htmlFor).toBe(input.id);
  });

  it("links the label to the checkbox inside a Switch", async () => {
    await render(
      <Field label="Автопилот">
        <Switch checked={false} onChange={() => {}} />
      </Field>,
    );
    const input = controlBox().querySelector<HTMLInputElement>(".switch input")!;
    expect(input.id).not.toBe("");
    expect(fieldLabel().htmlFor).toBe(input.id);
  });

  it("activates the control when the label text is clicked", async () => {
    const onClick = vi.fn();
    await render(
      <Field label="Имя">
        <input type="text" defaultValue="x" onClick={onClick} />
      </Field>,
    );
    act(() => {
      fieldLabel().click();
    });
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("toggles the switch when the label text is clicked", async () => {
    const onChange = vi.fn();
    await render(
      <Field label="Автопилот">
        <Switch checked={false} onChange={onChange} />
      </Field>,
    );
    act(() => {
      fieldLabel().click();
    });
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("leaves the label unlinked when the control is not a single field", async () => {
    await render(
      <Field label="Расположение">
        <div className="path-value">/tmp</div>
      </Field>,
    );
    expect(fieldLabel().hasAttribute("for")).toBe(false);
    expect(controlBox().querySelector("div")!.hasAttribute("id")).toBe(false);
  });

  it("leaves the label unlinked for several controls and for no control", async () => {
    await render(
      <Field label="Масштаб">
        <input type="text" />
        <button type="button">Сбросить</button>
      </Field>,
    );
    expect(fieldLabel().hasAttribute("for")).toBe(false);
    await render(<Field label="Temperature" hint="Фиксирована" />);
    expect(fieldLabel().hasAttribute("for")).toBe(false);
    expect(container.querySelector(".set-field-control")).toBeNull();
  });

  it("gives each field its own id", async () => {
    await render(
      <>
        <Field label="Первое">
          <input type="text" />
        </Field>
        <Field label="Второе">
          <input type="text" />
        </Field>
      </>,
    );
    const inputs = container.querySelectorAll("input");
    const labels = container.querySelectorAll<HTMLLabelElement>(".set-field-label");
    expect(inputs[0].id).not.toBe(inputs[1].id);
    expect(labels[0].htmlFor).toBe(inputs[0].id);
    expect(labels[1].htmlFor).toBe(inputs[1].id);
  });
});
