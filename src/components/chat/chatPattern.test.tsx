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

describe("panel headers share one height", () => {
  const css = readFileSync("src/styles/app.css", "utf8");

  function ruleFor(selector: string): string {
    const rule = css.match(new RegExp(`(?:^|\\n)\\${selector} \\{[^}]*\\}`));
    expect(rule, `the "${selector}" rule must exist`).not.toBeNull();
    return rule![0];
  }

  it("tokens.css defines the shared panel header height", () => {
    expect(readFileSync("src/styles/tokens.css", "utf8")).toContain("--panel-head-h: 56px;");
  });

  it("center-head and chat-head both use the shared height token", () => {
    for (const selector of [".center-head", ".chat-head"]) {
      const rule = ruleFor(selector);
      expect(rule, `${selector} must use the shared height`).toContain("min-height: var(--panel-head-h)");
      expect(rule, `${selector} must include the border in its height`).toContain("box-sizing: border-box");
      expect(rule, `${selector} must center its content`).toContain("align-items: center");
    }
  });

  it("header titles truncate with ellipsis instead of wrapping", () => {
    for (const selector of [".center-head h2", ".chat-title"]) {
      const rule = ruleFor(selector);
      expect(rule, `${selector} must not wrap`).toContain("white-space: nowrap");
      expect(rule, `${selector} must truncate`).toContain("text-overflow: ellipsis");
      expect(rule, `${selector} must be allowed to shrink`).toContain("min-width: 0");
    }
  });

  it("chat-pattern chat headers stay opaque white above the pattern", () => {
    const rule = css.match(/\.chat\.chat-pattern \.chat-head \{[^}]*\}/);
    expect(rule, "the chat-pattern chat-head rule must exist").not.toBeNull();
    expect(rule![0]).toContain("background: var(--surface)");
  });
});
