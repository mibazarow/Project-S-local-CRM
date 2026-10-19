import { describe, expect, it } from "vitest";
import { confirmAction, confirmRemove } from "./confirm";

// The dialog itself is covered by components/shared/ConfirmDialogHost.test.tsx;
// here the contract is the no-host fallback: the promise must resolve false
// instead of hanging forever (e.g. in tests or before the app root mounts).
describe("confirm channel without a mounted host", () => {
  it("confirmAction resolves false", async () => {
    await expect(confirmAction({ title: "Удалить файл?" })).resolves.toBe(false);
  });

  it("confirmRemove resolves false", async () => {
    await expect(confirmRemove({ title: "Удалить файл «notes.md»?" })).resolves.toBe(false);
  });
});
