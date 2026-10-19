import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import LlmTab from "./LlmTab";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const providers = () => [
  { id: "a", name: "DeepSeek", baseUrl: "https://api.deepseek.com/v1", apiKey: "sk-secret", active: true },
  { id: "b", name: "Local", baseUrl: "http://localhost:11434/v1", apiKey: "", active: false },
];

const invoked = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);
const argsOf = (cmd: string) => invoked(cmd).at(-1)?.[1] as Record<string, unknown> | undefined;

let container: HTMLDivElement;
let root: Root;
let listModelsResult: { id: string }[];

beforeEach(() => {
  listModelsResult = [{ id: "deepseek-chat" }];
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: unknown) => {
    switch (cmd) {
      case "read_config":
        return {
          model: "deepseek-chat",
          contextWindow: 65536,
          maxTokens: null,
          vision: null,
          thinkingLevel: "medium",
        };
      case "llm_providers_list":
        return providers();
      case "llm_provider_activate":
        return providers().map((p) => ({ ...p, active: p.id === (args as { id?: string })?.id }));
      case "llm_provider_update":
        return providers();
      case "llm_provider_add": {
        const a = args as { name?: string; baseUrl?: string; apiKey?: string };
        return [
          ...providers(),
          {
            id: "c",
            name: a.name ?? "",
            baseUrl: a.baseUrl ?? "",
            apiKey: a.apiKey ?? "",
            active: false,
          },
        ];
      }
      case "llm_provider_delete":
        return providers().filter((p) => p.id !== (args as { id?: string })?.id);
      case "test_connection":
        return { ok: true, message: "ok", modelCount: 2 };
      case "list_models":
        return listModelsResult;
      case "get_vision_status":
        return { supported: null, overridden: false, reason: null };
      case "write_config":
        return undefined;
      default:
        return undefined;
    }
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function mount() {
  await act(async () => {
    root.render(<LlmTab active error={null} setError={() => {}} />);
  });
  await act(async () => {});
}

const text = () => container.textContent ?? "";

const btn = (label: string) =>
  Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
    (b) => b.textContent?.trim() === label,
  )!;

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

function typeInput(input: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  setter.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

function selectValue(select: HTMLSelectElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!;
  setter.call(select, value);
  select.dispatchEvent(new Event("change", { bubbles: true }));
}

async function submit() {
  const form = container.querySelector("form.set-form")!;
  await act(async () => {
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  });
}

describe("LlmTab providers", () => {
  it("lists saved providers with host and active marker, no quick-switcher", async () => {
    await mount();
    expect(text()).toContain("DeepSeek");
    expect(text()).toContain("Local");
    expect(text()).toContain("api.deepseek.com");
    expect(text()).toContain("активен");
    // Everything lives inside the settings content; there is no header switcher.
    expect(container.querySelectorAll(".settings-content")).toHaveLength(1);
    expect(container.querySelector(".topbar, .quick-switcher, .provider-switch")).toBeNull();
  });

  it("masks the API key in the editor and toggles reveal", async () => {
    await mount();
    await click(btn("Изменить"));
    const key = container.querySelector<HTMLInputElement>(".llm-key-row input")!;
    expect(key.type).toBe("password");
    expect(key.value).toBe("sk-secret");
    await click(btn("Показать"));
    expect(container.querySelector<HTMLInputElement>(".llm-key-row input")!.type).toBe("text");
  });

  it("activates an inactive provider via its radio", async () => {
    await mount();
    const radios = Array.from(container.querySelectorAll<HTMLButtonElement>(".provider-radio"));
    await click(radios[1]);
    expect(argsOf("llm_provider_activate")?.id).toBe("b");
  });

  it("saves and activates the edited provider before testing", async () => {
    await mount();
    const edit = Array.from(
      container.querySelectorAll<HTMLButtonElement>(".provider-item button"),
    ).filter((b) => b.textContent?.trim() === "Изменить");
    await click(edit[1]);
    const key = container.querySelector<HTMLInputElement>(".llm-key-row input")!;
    typeInput(key, "sk-local");
    await click(btn("Проверить подключение"));
    await act(async () => {});
    expect(argsOf("llm_provider_update")?.id).toBe("b");
    expect(argsOf("llm_provider_update")?.apiKey).toBe("sk-local");
    expect(argsOf("llm_provider_activate")?.id).toBe("b");
    const order = invokeMock.mock.calls.map((c) => c[0]);
    expect(order.indexOf("llm_provider_update")).toBeLessThan(
      order.indexOf("llm_provider_activate"),
    );
    expect(order.indexOf("llm_provider_activate")).toBeLessThan(
      order.indexOf("test_connection"),
    );
  });

  it("applies the DeepSeek preset to the draft and model", async () => {
    await mount();
    await click(btn("+ Добавить провайдера"));
    const template = container.querySelector<HTMLSelectElement>("form.set-form select")!;
    await act(async () => {
      selectValue(template, "deepseek");
    });
    const name = container.querySelector<HTMLInputElement>('input[placeholder="DeepSeek"]')!;
    const url = container.querySelector<HTMLInputElement>(
      'input[placeholder="https://api.example.com/v1"]',
    )!;
    const model = container.querySelector<HTMLInputElement>('[role="combobox"]')!;
    expect(name.value).toBe("DeepSeek");
    expect(url.value).toBe("https://api.deepseek.com/v1");
    expect(model.value).toBe("deepseek-chat");
    expect(template.value).toBe("");
  });

  it("adds a provider with trimmed fields", async () => {
    await mount();
    await click(btn("+ Добавить провайдера"));
    const name = container.querySelector<HTMLInputElement>('input[placeholder="DeepSeek"]')!;
    const url = container.querySelector<HTMLInputElement>(
      'input[placeholder="https://api.example.com/v1"]',
    )!;
    const key = container.querySelector<HTMLInputElement>(".llm-key-row input")!;
    typeInput(name, "  OpenAI  ");
    typeInput(url, "  https://api.openai.com/v1  ");
    typeInput(key, "sk-x");
    await submit();
    const a = argsOf("llm_provider_add");
    expect(a?.name).toBe("OpenAI");
    expect(a?.baseUrl).toBe("https://api.openai.com/v1");
    expect(a?.apiKey).toBe("sk-x");
  });

  it("deletes a provider after confirmation", async () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
    await mount();
    await click(btn("Удалить"));
    expect(confirmSpy).toHaveBeenCalled();
    expect(argsOf("llm_provider_delete")?.id).toBe("a");
    confirmSpy.mockRestore();
  });

  it("keeps the model combobox and vision control working", async () => {
    await mount();
    expect(container.querySelector('[role="combobox"]')).not.toBeNull();
    const selects = Array.from(container.querySelectorAll("select"));
    const values = selects.map((s) => (s as HTMLSelectElement).value);
    expect(values).toContain("medium");
    expect(values).toContain("auto");
  });
});

describe("LlmTab model combobox", () => {
  const combobox = () => container.querySelector<HTMLInputElement>('[role="combobox"]')!;
  const options = () =>
    Array.from(container.querySelectorAll<HTMLElement>(".model-option")).map(
      (o) => o.textContent,
    );

  async function loadModels(ids: string[]) {
    listModelsResult = ids.map((id) => ({ id }));
    await mount();
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
  }

  async function focusCombobox() {
    await act(async () => {
      combobox().focus();
    });
  }

  it("shows all loaded models even when the current value is absent", async () => {
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    expect(combobox().value).toBe("deepseek-chat");
    await focusCombobox();
    expect(options()).toEqual(["deepseek-flash", "deepseek-v4-pro"]);
  });

  it("picks a model from the dropdown", async () => {
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    await focusCombobox();
    const option = Array.from(
      container.querySelectorAll<HTMLElement>(".model-option"),
    ).find((o) => o.textContent === "deepseek-flash")!;
    await click(option);
    expect(combobox().value).toBe("deepseek-flash");
    expect(container.querySelector(".model-dropdown")).toBeNull();
  });

  it("filters models by the typed text", async () => {
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    await focusCombobox();
    await act(async () => {
      typeInput(combobox(), "v4");
    });
    expect(options()).toEqual(["deepseek-v4-pro"]);
  });
});
