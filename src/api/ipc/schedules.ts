// Scheduler IPC (SPEC 5.3) and the schedules-changed/schedule-open events.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ScheduleDto, ScheduleOpenEvent } from "../types";

// Scheduler (SPEC 5.3): run_at / cron / notify over the schedules table.
// Creation goes through the agent's schedule tools from a chat, not an IPC form.
export const scheduleList = () => invoke<ScheduleDto[]>("schedule_list");

export const schedulePause = (id: number) => invoke<ScheduleDto>("schedule_pause", { id });

export const scheduleResume = (id: number) => invoke<ScheduleDto>("schedule_resume", { id });

export const scheduleDelete = (id: number) => invoke<void>("schedule_delete", { id });

// Click-through: returns the session whose chat the UI opens (the schedule's
// own, or a fresh one titled after the schedule created by the backend).
export const scheduleOpen = (id: number) =>
  invoke<{ sessionId: number }>("schedule_open", { id });

export const onSchedulesChanged = (handler: () => void) =>
  listen<unknown>("schedules-changed", () => handler());

// Emitted when the UI must switch to a session's chat: the notification click
// handler in lib.rs (a schedule-run completion) and the schedule_create tool
// (a freshly created schedule chat, Q.md item 4).
export const onScheduleOpen = (handler: (payload: ScheduleOpenEvent) => void) =>
  listen<ScheduleOpenEvent>("schedule-open", (e) => handler(e.payload));
