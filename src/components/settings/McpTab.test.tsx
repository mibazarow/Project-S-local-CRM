import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { McpServerStatus } from "../../api/types";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
}));

import McpTab from "./McpTab";
import ConfirmDialogHost from "../shared/ConfirmDialogHost";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const status = (name: string, o: Partial<McpServerStatus>): McpServerStatus => ({
  name,
  enabled: true,
  transport: "stdio",
  status: "running",
  error: null,
  toolCount: 0,
  ...o,
});

let statusRows: McpServerStatus[];
let rawMcp: Record<string, unknown>;
let reloadRows: McpServerStatus[];

const invoked = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  statusRows = [
    status("one", { toolCount: 3 }),
    status("two", { transport: "http", status: "error", error: "connection error: refused" }),
    status("off", { enabled: false, status: "disabled" }),
  ];
  rawMcp = {
    servers: [
      {
        name: "one",
        enabled: true,
        timeoutSecs: 30,
        transport: { type: "stdio", command: "npx", args: ["-y", "@example/mcp"], env: { API_KEY: "x" } },
      },
      { name: "two", enabled: true, transport: { type: "http", url: "https://mcp.example.com/mcp" } },
      { name: "off", enabled: false, transport: { type: "stdio", command: "node" } },
    ],
  };
  reloadRows = statusRows;
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "mcp_status":
        return statusRows;
      case "mcp_reload":
        return reloadRows;
      case "read_config":
        return rawMcp;
      case "write_config":
        return undefined;
      case "mcp_set_enabled":
        return statusRows.map((s) => ({ ...s, enabled: false, status: "disabled" as const }));
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
        <McpTab active />
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

function fieldControl(label: string): HTMLElement {
  const field = Array.from(container.querySelectorAll<HTMLElement>(".set-field")).find(
    (f) => f.querySelector(".set-field-label")?.textContent === label,
  );
  if (!field) throw new Error(`field not found: ${label}`);
  return field.querySelector(".set-field-control")!;
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
  await act(async () => {
    container.querySelector("form")!.dispatchEvent(
      new Event("submit", { bubbles: true, cancelable: true }),
    );
  });
}

const extras = () => container.querySelector("details.set-form-details")! as HTMLDetailsElement;

describe("McpTab server list", () => {
  it("renders transport, status, tool count and error per server", async () => {
    await mount();
    expect(text()).toContain("инструментов: 3");
    expect(text()).toContain("connection error: refused");
    expect(text()).toContain("выключен");
    const badges = Array.from(container.querySelectorAll(".mcp-item .badge.st-error"));
    expect(badges).toHaveLength(1);
    expect(badges[0].textContent).toBe("ошибка");
  });

  it("toggle off calls mcp_set_enabled and refreshes rows from its result", async () => {
    await mount();
    const first = container.querySelector<HTMLInputElement>(".mcp-item .switch input")!;
    expect(first.checked).toBe(true);
    await act(async () => {
      first.click();
    });
    const calls = invoked("mcp_set_enabled");
    expect(calls).toHaveLength(1);
    expect(calls[0][1]).toEqual({ name: "one", enabled: false });
    expect(invoked("mcp_reload")).toHaveLength(0);
    expect(text()).toContain("выключен");
  });

  it("delete asks through the app modal and only then rewrites mcp.json", async () => {
    await mount();
    const items = container.querySelectorAll(".mcp-item");
    await click(items[0].querySelectorAll("button").item(1) as HTMLButtonElement);
    const dialog = document.body.querySelector(".confirm-dialog")!;
    expect(dialog.textContent).toContain("Удалить сервер «one»?");
    expect(invoked("write_config")).toHaveLength(0);
    await click(dialog.querySelector<HTMLButtonElement>(".ghost-btn")!);
    expect(invoked("write_config")).toHaveLength(0);

    await click(items[0].querySelectorAll("button").item(1) as HTMLButtonElement);
    const dialog2 = document.body.querySelector(".confirm-dialog")!;
    await click(dialog2.querySelector<HTMLButtonElement>(".primary-btn")!);
    const writes = invoked("write_config");
    expect(writes).toHaveLength(1);
    const servers = (writes[0][1] as { value: unknown }).value;
    expect(servers).toMatchObject({
      servers: [{ name: "two" }, { name: "off" }],
    });
    expect(invoked("mcp_reload")).toHaveLength(1);
  });
});

describe("McpTab form progressive disclosure", () => {
  it("shows command for stdio and url for http, extras stay behind the details", async () => {
    await mount();
    expect(fieldControl("Команда").querySelector("input")).toBeTruthy();
    expect(text()).not.toContain("URL сервера");
    expect(extras().open).toBe(false);

    const transport = container.querySelector<HTMLSelectElement>("form .set-form-section select")!;
    await act(async () => {
      selectValue(transport, "http");
    });
    expect(fieldControl("URL сервера").querySelector("input")).toBeTruthy();
    expect(text()).not.toContain("Команда");
    expect(text()).not.toContain("Переменные окружения");
    expect(fieldControl("Заголовки (необязательно)")).toBeTruthy();
    expect(fieldControl("Таймаут вызова, сек (необязательно)")).toBeTruthy();
  });

  it("edit pre-fills the form and auto-opens the extras section", async () => {
    await mount();
    await click(btn("Изменить"));
    await act(async () => {});
    expect(extras().open).toBe(true);
    expect((fieldControl("Таймаут вызова, сек (необязательно)").querySelector("input") as HTMLInputElement).value).toBe("30");
    expect((fieldControl("Команда").querySelector("input") as HTMLInputElement).value).toBe("npx");
    expect(text()).toContain("Изменить сервер «one»");
  });

  it("rejects a non-integer timeout without writing", async () => {
    await mount();
    typeInput(fieldControl("Имя сервера").querySelector("input") as HTMLInputElement, "new-srv");
    typeInput(fieldControl("Команда").querySelector("input") as HTMLInputElement, "npx");
    await act(async () => {
      extras().open = true;
      extras().dispatchEvent(new Event("toggle", { bubbles: true }));
    });
    typeInput(fieldControl("Таймаут вызова, сек (необязательно)").querySelector("input") as HTMLInputElement, "0");
    await submit();
    expect(text()).toContain("Таймаут — целое число секунд от 1 до 3600");
    expect(invoked("write_config")).toHaveLength(0);
  });

  it("saves a new stdio server with timeoutSecs and reloads", async () => {
    await mount();
    typeInput(fieldControl("Имя сервера").querySelector("input") as HTMLInputElement, "new-srv");
    typeInput(fieldControl("Команда").querySelector("input") as HTMLInputElement, "npx");
    await act(async () => {
      extras().open = true;
      extras().dispatchEvent(new Event("toggle", { bubbles: true }));
    });
    typeInput(fieldControl("Таймаут вызова, сек (необязательно)").querySelector("input") as HTMLInputElement, "45");
    await submit();
    const writes = invoked("write_config");
    expect(writes).toHaveLength(1);
    const servers = (writes[0][1] as { value: { servers: Record<string, unknown>[] } }).value.servers;
    expect(servers[servers.length - 1]).toMatchObject({
      name: "new-srv",
      enabled: true,
      timeoutSecs: 45,
      transport: { type: "stdio", command: "npx", args: [] },
    });
    expect(invoked("mcp_reload")).toHaveLength(1);
  });
});
