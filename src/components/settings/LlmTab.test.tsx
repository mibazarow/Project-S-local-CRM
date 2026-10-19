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

beforeEach(() => {
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
      case "llm_provider_add":
      case "llm_provider_update":
      case "llm_provider_delete":
        return providers().map((p) => ({ ...p, active: p.id === (args as { id?: string })?.id }));
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
