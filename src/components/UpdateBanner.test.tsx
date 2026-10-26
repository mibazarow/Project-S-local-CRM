import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const checkUpdate = vi.hoisted(() => vi.fn());

vi.mock("../api/ipc", () => ({ checkUpdate }));
vi.mock("../state/AppStore", () => ({
  useApp: () => ({ state: { onboarding: { configured: true } } }),
}));

import UpdateBanner from "./UpdateBanner";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  localStorage.clear();
  checkUpdate.mockReset();
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
    root.render(<UpdateBanner />);
  });
}

describe("UpdateBanner", () => {
  it("shows the available version, release link and keeps data on upgrade", async () => {
    checkUpdate.mockResolvedValue({
      current: "1.1.0",
      latest: "1.1.1",
      url: "https://gitverse.ru/mibazarow/Project-S-local-CRM/releases/tag/v1.1.1",
      available: true,
      checkedAt: "2026-10-26T12:00:00Z",
      cached: false,
    });
    await mount();
    expect(container.textContent).toContain("Доступна новая версия Project-S 1.1.1");
    expect(container.textContent).toContain("поверх текущей");
    expect(container.querySelector("a")?.getAttribute("href")).toContain("/releases/tag/v1.1.1");
    expect(container.querySelector("a")?.textContent).toBe("Релизы");
  });

  it("does not show anything when the installed version is current", async () => {
    checkUpdate.mockResolvedValue({
      current: "1.1.0",
      latest: "1.1.0",
      url: null,
      available: false,
      checkedAt: "2026-10-26T12:00:00Z",
      cached: false,
    });
    await mount();
    expect(container.querySelector(".update-banner")).toBeNull();
  });

  it("dismisses the same version until a newer release appears", async () => {
    checkUpdate.mockResolvedValue({
      current: "1.1.0",
      latest: "1.1.1",
      url: "https://example.test/releases/tag/v1.1.1",
      available: true,
      checkedAt: "2026-10-26T12:00:00Z",
      cached: false,
    });
    await mount();
    await act(async () => {
      container.querySelector<HTMLButtonElement>(".update-banner-close")!.click();
    });
    expect(container.querySelector(".update-banner")).toBeNull();
    expect(localStorage.getItem("project-s.update.dismissed")).toBe("1.1.1");
  });
});
