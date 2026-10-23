import { readAppCss } from "../../styles/css-source";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ScheduleDto } from "../../api/types";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import SchedulePanel from "./SchedulePanel";

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

const makeSchedule = (overrides: Partial<ScheduleDto> = {}): ScheduleDto => ({
  id: 1,
  sessionId: 7,
  kind: "cron",
  spec: "0 9 * * *",
  prompt: null,
  status: "active",
  relatedTaskId: null,
  createdAt: "2026-01-01 09:00:00",
  lastRunAt: null,
  nextRunAt: "2026-10-21T09:00:00",
  timezone: "Europe/Moscow",
  error: null,
  title: "Ежедневный отчёт",
  unreadAt: null,
  ...overrides,
});

async function mount(schedules: ScheduleDto[]) {
  await act(async () => {
    root.render(
      <SchedulePanel
        schedules={schedules}
        schedulesError={null}
        onToggle={vi.fn()}
        onDelete={vi.fn()}
        onOpen={vi.fn()}
        onCreate={vi.fn()}
      />,
    );
  });
}

describe("SchedulePanel card (Q.md items 5-6)", () => {
  it("renders no status badge and no status text in the card", async () => {
    await mount([
      makeSchedule(),
      makeSchedule({ id: 2, status: "paused", title: "Пауза-задача" }),
      makeSchedule({ id: 3, status: "done", title: "Готово-задача" }),
    ]);
    const items = container.querySelectorAll(".schedule-item");
    expect(items).toHaveLength(3);
    expect(container.querySelectorAll(".schedule-item .badge")).toHaveLength(0);
    for (const item of items) {
      expect(item.textContent).not.toContain("активно");
      expect(item.textContent).not.toContain("пауза");
      expect(item.textContent).not.toContain("ошибка");
    }
  });

  it("keeps the toggle reflecting the status: active checked, paused unchecked, done disabled", async () => {
    await mount([
      makeSchedule(),
      makeSchedule({ id: 2, status: "paused" }),
      makeSchedule({ id: 3, status: "done" }),
    ]);
    const boxes = Array.from(
      container.querySelectorAll<HTMLInputElement>(".schedule-item .switch input"),
    );
    expect(boxes).toHaveLength(3);
    expect(boxes[0].checked).toBe(true);
    expect(boxes[0].disabled).toBe(false);
    expect(boxes[1].checked).toBe(false);
    expect(boxes[2].checked).toBe(false);
    expect(boxes[2].disabled).toBe(true);
  });

  it("keeps the subtitle with kind, timezone and next run", async () => {
    await mount([makeSchedule()]);
    const caption = container.querySelector(".schedule-item .info .c")!;
    expect(caption.textContent).toContain("Cron");
    expect(caption.textContent).toContain("Europe/Moscow");
    expect(caption.textContent).toContain("след.:");
  });

  it("keeps the separate error line for an error schedule", async () => {
    await mount([makeSchedule({ status: "error", error: "Сбой запуска" })]);
    const captions = container.querySelectorAll(".schedule-item .info .c");
    expect(captions).toHaveLength(2);
    expect(captions[1].textContent).toBe("Сбой запуска");
  });

  it("the stylesheet paints the card white with a border and shadow on the panel", () => {
    const css = readAppCss();
    const rule = css.match(/\.schedule-item \{[^}]*\}/);
    expect(rule, "the schedule card rule must exist").not.toBeNull();
    expect(rule![0]).toContain("background: var(--surface)");
    expect(rule![0]).not.toContain("var(--surface-2)");
    expect(rule![0]).toContain("border: 1px solid var(--border-strong)");
    expect(rule![0]).toContain("box-shadow:");
  });

  it("marks an unread schedule with the pulse class and leaves read ones plain", async () => {
    await mount([
      makeSchedule({ id: 2, unreadAt: "2026-10-20T10:00:00Z" }),
      makeSchedule({ id: 3 }),
    ]);
    const items = container.querySelectorAll<HTMLElement>(".schedule-item");
    expect(items).toHaveLength(2);
    expect(items[0].classList.contains("unread")).toBe(true);
    expect(items[1].classList.contains("unread")).toBe(false);
  });

  it("the unread class clears when the reloaded list arrives without unreadAt", async () => {
    await mount([makeSchedule({ unreadAt: "2026-10-20T10:00:00Z" })]);
    expect(container.querySelector(".schedule-item")!.classList.contains("unread")).toBe(true);
    // Opening the schedule clears unread_at in the backend and the reloaded
    // DTO comes back read: the pulse must be gone.
    await mount([makeSchedule({ unreadAt: null })]);
    expect(container.querySelector(".schedule-item")!.classList.contains("unread")).toBe(false);
  });

  it("the stylesheet defines a soft box-shadow pulse for the unread card", () => {
    const css = readAppCss();
    const rule = css.match(/\.schedule-item\.unread \{[^}]*\}/);
    expect(rule, "the unread rule must exist").not.toBeNull();
    expect(rule![0]).toContain("animation:");
    const keyframes = css.match(/@keyframes schedule-unread-pulse \{[\s\S]*?\n\}/);
    expect(keyframes, "the pulse keyframes must exist").not.toBeNull();
    expect(keyframes![0]).toContain("box-shadow:");
  });
});
