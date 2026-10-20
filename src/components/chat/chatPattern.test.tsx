import { readFileSync } from "node:fs";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

import SchedulePanel from "../harness/SchedulePanel";

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

describe("chat background pattern", () => {
  it("project and notes chat roots carry chat-pattern in both drag-over states", () => {
    for (const file of ["src/components/chat/ProjectChat.tsx", "src/components/notes/NotesChat.tsx"]) {
      const src = readFileSync(file, "utf8");
      expect(src, `${file} must keep the conditional drag-over class`).toContain(
        'className={att.dragOver ? "chat chat-pattern drag-over" : "chat chat-pattern"}',
      );
    }
  });

  it("the schedule panel aside stays a plain chat without chat-pattern", async () => {
    await act(async () => {
      root.render(
        <SchedulePanel
          schedules={[]}
          schedulesError={null}
          onToggle={vi.fn()}
          onDelete={vi.fn()}
          onOpen={vi.fn()}
          onCreate={vi.fn()}
        />,
      );
    });
    const aside = container.querySelector("aside.chat")!;
    expect(aside).not.toBeNull();
    expect(aside.classList.contains("chat-pattern")).toBe(false);
  });

  it("the stylesheet paints the pattern full width repeating downward on chat-pattern", () => {
    const css = readFileSync("src/styles/app.css", "utf8");
    const rule = css.match(/\.chat\.chat-pattern \{[^}]*\}/);
    expect(rule, "the chat-pattern rule must exist").not.toBeNull();
    expect(rule![0]).toContain('background-image: url("../assets/pattern.png")');
    expect(rule![0]).toContain("background-repeat: repeat-y");
    expect(rule![0]).toContain("background-size: 100% auto");
  });
});
