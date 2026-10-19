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
import ConfirmDialogHost from "../shared/ConfirmDialogHost";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let activeId = "a";

const providers = () => [
  { id: "a", name: "DeepSeek", baseUrl: "https://api.deepseek.com/v1", apiKey: "sk-secret", active: activeId === "a" },
  { id: "b", name: "Local", baseUrl: "http://localhost:11434/v1", apiKey: "", active: activeId === "b" },
];

const invoked = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);
const argsOf = (cmd: string) => invoked(cmd).at(-1)?.[1] as Record<string, unknown> | undefined;

let container: HTMLDivElement;
let root: Root;
let listModelsResult: { id: string }[];
let listModelsB: { id: string }[];
let listModelsFails: boolean;
let configModel: string;
let configLastModelByProvider: Record<string, string>;

beforeEach(() => {
  activeId = "a";
  listModelsResult = [{ id: "deepseek-chat" }];
  listModelsB = [{ id: "llama-3.1" }, { id: "qwen-max" }];
  listModelsFails = false;
  configModel = "deepseek-chat";
  configLastModelByProvider = {};
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: unknown) => {
    switch (cmd) {
      case "read_config":
        return {
          model: configModel,
          contextWindow: 65536,
          maxTokens: null,
          vision: null,
          thinkingLevel: "medium",
          lastModelByProvider: configLastModelByProvider,
        };
      case "llm_providers_list":
        return providers();
      case "llm_provider_activate": {
        const id = (args as { id?: string })?.id;
        if (id) activeId = id;
        return providers();
      }
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
        if (listModelsFails) throw new Error("models failed");
        return activeId === "b" ? listModelsB : listModelsResult;
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
    root.render(
      <>
        <LlmTab active error={null} setError={() => {}} />
        <ConfirmDialogHost />
      </>,
    );
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
    expect(name.value).toBe("DeepSeek");
    expect(url.value).toBe("https://api.deepseek.com/v1");
    expect(template.value).toBe("");
    typeInput(container.querySelector<HTMLInputElement>(".llm-key-row input")!, "sk-x");
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
    const model = container.querySelector<HTMLSelectElement>(".model-row select")!;
    expect(model.value).toBe("deepseek-chat");
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

  it("deletes a provider only after the in-app confirmation", async () => {
    await mount();
    await click(btn("Удалить"));
    const dialog = document.body.querySelector(".confirm-dialog")!;
    expect(dialog.textContent).toContain("Удалить провайдера «DeepSeek»?");
    expect(invoked("llm_provider_delete")).toHaveLength(0);
    await click(dialog.querySelector<HTMLButtonElement>(".primary-btn")!);
    expect(argsOf("llm_provider_delete")?.id).toBe("a");
  });

  it("keeps the provider when the confirmation is cancelled", async () => {
    await mount();
    await click(btn("Удалить"));
    const dialog = document.body.querySelector(".confirm-dialog")!;
    await click(dialog.querySelector<HTMLButtonElement>(".ghost-btn")!);
    expect(document.body.querySelector(".confirm-dialog")).toBeNull();
    expect(invoked("llm_provider_delete")).toHaveLength(0);
  });

  it("keeps the model picker and vision control working", async () => {
    await mount();
    expect(container.querySelector(".model-row select")).not.toBeNull();
    const selects = Array.from(container.querySelectorAll("select"));
    const values = selects.map((s) => (s as HTMLSelectElement).value);
    expect(values).toContain("medium");
    expect(values).toContain("auto");
  });
});

describe("LlmTab model picker", () => {
  const modelRow = () => container.querySelector<HTMLElement>(".model-row")!;
  const modelSelect = () => modelRow().querySelector<HTMLSelectElement>("select");
  const modelInput = () => modelRow().querySelector<HTMLInputElement>("input");
  const options = () => Array.from(modelSelect()?.options ?? []);

  async function loadModels(ids: string[]) {
    listModelsResult = ids.map((id) => ({ id }));
    await mount();
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
  }

  it("shows loaded models in a select and no manual input", async () => {
    configModel = "";
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    expect(modelInput()).toBeNull();
    expect(options().filter((o) => !o.disabled).map((o) => o.value)).toEqual([
      "deepseek-flash",
      "deepseek-v4-pro",
    ]);
    expect(options()[0].textContent).toBe("Выбери модель");
    expect(options()[0].disabled).toBe(true);
  });

  it("keeps an out-of-list current model as a current option", async () => {
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    expect(modelSelect()!.value).toBe("deepseek-chat");
    expect(options().map((o) => o.textContent)).toEqual([
      "deepseek-chat (текущая)",
      "deepseek-flash",
      "deepseek-v4-pro",
    ]);
    expect(modelInput()).toBeNull();
  });

  it("selects a model and saves it through the debounced config", async () => {
    await loadModels(["deepseek-flash", "deepseek-v4-pro"]);
    const select = modelSelect()!;
    await act(async () => {
      select.focus();
      selectValue(select, "deepseek-flash");
      select.blur();
    });
    await act(async () => {});
    expect(select.value).toBe("deepseek-flash");
    expect((argsOf("write_config")?.value as { model?: string }).model).toBe("deepseek-flash");
  });

  it("falls back to manual entry when the endpoint returns no models", async () => {
    await loadModels([]);
    expect(modelSelect()).toBeNull();
    expect(modelInput()).not.toBeNull();
    expect(text()).toContain(
      "К сожалению, ваш endpoint не вернул список доступных моделей. Внесите название вручную. Уточните у провайдера.",
    );
  });

  it("falls back to manual entry when the model list request fails", async () => {
    listModelsFails = true;
    await mount();
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
    expect(modelSelect()).toBeNull();
    expect(modelInput()).not.toBeNull();
    expect(text()).toContain(
      "К сожалению, ваш endpoint не вернул список доступных моделей. Внесите название вручную. Уточните у провайдера.",
    );
  });
});

describe("LlmTab model memory per provider", () => {
  const modelSelect = () =>
    container.querySelector<HTMLElement>(".model-row")?.querySelector<HTMLSelectElement>("select") ??
    null;
  const options = () => Array.from(modelSelect()?.options ?? []);

  async function activate(id: "a" | "b") {
    const radios = Array.from(container.querySelectorAll<HTMLButtonElement>(".provider-radio"));
    await click(radios[id === "a" ? 0 : 1]);
  }

  async function loadModels() {
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
  }

  async function flushWrite() {
    const select = modelSelect()!;
    await act(async () => {
      select.focus();
      select.blur();
    });
    await act(async () => {});
    return argsOf("write_config")?.value as Record<string, unknown>;
  }

  it("on provider switch loads new models and selects the remembered model", async () => {
    configLastModelByProvider = { b: "qwen-max" };
    await mount();
    await activate("b");
    await loadModels();
    expect(modelSelect()!.value).toBe("qwen-max");
    const written = await flushWrite();
    expect(written.model).toBe("qwen-max");
    expect((written.lastModelByProvider as Record<string, string>).b).toBe("qwen-max");
  });

  it("without history selects the first model of the new provider and remembers it", async () => {
    await mount();
    await activate("b");
    await loadModels();
    expect(modelSelect()!.value).toBe("llama-3.1");
    const written = await flushWrite();
    expect(written.model).toBe("llama-3.1");
    expect((written.lastModelByProvider as Record<string, string>).b).toBe("llama-3.1");
  });

  it("when the remembered model is gone from the list selects the first model", async () => {
    configLastModelByProvider = { b: "gone-model" };
    await mount();
    await activate("b");
    await loadModels();
    expect(modelSelect()!.value).toBe("llama-3.1");
    expect(options().map((o) => o.textContent)).toEqual(["llama-3.1", "qwen-max"]);
  });

  it("selecting a model records it in lastModelByProvider of the config write", async () => {
    await mount();
    await loadModels();
    listModelsResult = [{ id: "deepseek-chat" }, { id: "deepseek-r1" }];
    await click(btn("Загрузить модели"));
    await act(async () => {});
    await act(async () => {});
    const sel = modelSelect()!;
    await act(async () => {
      sel.focus();
      selectValue(sel, "deepseek-r1");
      sel.blur();
    });
    await act(async () => {});
    const written = argsOf("write_config")?.value as Record<string, unknown>;
    expect(written.model).toBe("deepseek-r1");
    expect((written.lastModelByProvider as Record<string, string>).a).toBe("deepseek-r1");
  });

  it("switching providers never keeps the previous provider's models", async () => {
    await mount();
    await loadModels();
    expect(options().map((o) => o.value)).toContain("deepseek-chat");
    await activate("b");
    expect(options().map((o) => o.value)).not.toContain("deepseek-chat");
    await loadModels();
    expect(options().map((o) => o.value)).toEqual(["llama-3.1", "qwen-max"]);
    expect(modelSelect()!.value).toBe("llama-3.1");
  });
});
