import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppProvider } from "../../state/AppStore";

vi.mock("../../api/ipc", () => ({
  onboardingStatus: vi.fn(async () => ({ configured: false, workingDir: null })),
  listProjects: vi.fn(async () => []),
  licenseStatus: vi.fn(async () => ({ required: true, accepted: false, version: 1 })),
  licenseAccept: vi.fn(async () => ({ required: false, accepted: true, version: 1 })),
  licenseDecline: vi.fn(async () => undefined),
  defaultWorkingDir: vi.fn(async () => "/tmp/project-s"),
  getAutostart: vi.fn(async () => false),
  setAutostart: vi.fn(async () => false),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(async () => null),
}));

import * as ipc from "../../api/ipc";
import type { LicenseStatus } from "../../api/types";
import LicenseGate, { shouldShowLicense } from "./License";

const accepted: LicenseStatus = { required: false, accepted: true, version: 1 };
const pending: LicenseStatus = { required: true, accepted: false, version: 1 };

let container: HTMLDivElement;
let root: Root;

async function mount() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(
      <AppProvider>
        <LicenseGate />
      </AppProvider>,
    );
  });
}

const button = (label: string) =>
  Array.from(container.querySelectorAll("button")).find((b) => b.textContent === label)!;

beforeEach(() => {
  vi.mocked(ipc.licenseStatus).mockResolvedValue(pending);
  vi.mocked(ipc.licenseAccept).mockResolvedValue(accepted);
  vi.mocked(ipc.licenseDecline).mockClear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("license gate", () => {
  it("shows the agreement with the key disclaimers", async () => {
    await mount();
    const text = container.textContent ?? "";
    expect(text).toContain("Лицензионное соглашение и отказ от ответственности");
    expect(text).toContain("Полная ответственность пользователя");
    expect(text).toContain("полный доступ к вашему компьютеру");
    expect(text).toContain("SSH");
    expect(text).toContain("буфер обмена");
    expect(text).toContain("MCP");
    expect(text).toContain("«как есть»");
    expect(text).toContain("не несёт ответственности");
    expect(text).toContain("AI может ошибаться");
    expect(text).toContain("резервные копии");
    expect(text).toContain("законодательства");
  });

  it("gates the accept button on the checkbox", async () => {
    await mount();
    expect(button("Согласиться").disabled).toBe(true);

    const checkbox = container.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
    await act(async () => {
      checkbox.click();
    });
    expect(button("Согласиться").disabled).toBe(false);

    await act(async () => {
      checkbox.click();
    });
    expect(button("Согласиться").disabled).toBe(true);
  });

  it("accept persists acceptance and proceeds to onboarding", async () => {
    await mount();
    await act(async () => {
      container.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click();
    });
    await act(async () => {
      button("Согласиться").click();
    });
    expect(ipc.licenseAccept).toHaveBeenCalledTimes(1);
    expect(container.textContent).toContain("Добро пожаловать в Project-S");
  });

  it("decline takes the exit path", async () => {
    await mount();
    await act(async () => {
      button("Не согласиться").click();
    });
    expect(ipc.licenseDecline).toHaveBeenCalledTimes(1);
    expect(container.textContent).toContain("Лицензионное соглашение");
  });

  it("never gates a configured install and skips accepted installs", () => {
    expect(shouldShowLicense(true, pending)).toBe(false);
    expect(shouldShowLicense(true, accepted)).toBe(false);
    expect(shouldShowLicense(false, accepted)).toBe(false);
    expect(shouldShowLicense(false, pending)).toBe(true);
  });

  it("goes straight to onboarding when the license is already accepted", async () => {
    vi.mocked(ipc.licenseStatus).mockResolvedValue(accepted);
    await mount();
    expect(container.textContent).toContain("Добро пожаловать в Project-S");
    expect(container.textContent).not.toContain("Лицензионное соглашение");
  });
});
