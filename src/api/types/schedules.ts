// scheduler::ScheduleDto and the "schedule-open" navigation event.

export interface ScheduleDto {
  id: number;
  sessionId: number | null;
  kind: "run_at" | "cron" | "notify";
  spec: string;
  prompt: string | null;
  status: "active" | "paused" | "done" | "error";
  relatedTaskId: number | null;
  createdAt: string;
  lastRunAt: string | null;
  nextRunAt: string | null;
  timezone: string | null;
  error: string | null;
  title: string | null;
  // Q.md item 7: set when a scheduled run finished and its result has not
  // been opened yet; the schedule card pulses while it is set.
  unreadAt: string | null;
}

// "schedule-open" — emitted when the frontend must open a session's chat:
// a clicked schedule-run notification or a freshly created schedule chat.
export interface ScheduleOpenEvent {
  sessionId: number;
}
