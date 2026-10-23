// tasks::ChecklistItem, TaskDto, todos::TodoDto and their change events.

export interface ChecklistItem {
  text: string;
  done: boolean;
}

export interface TaskDto {
  id: number;
  projectId: number;
  title: string;
  status: string;
  comment: string | null;
  dueDate: string | null;
  dueTime: string;
  remindAt: string | null;
  position: number;
  createdAt: string;
  closedAt: string | null;
  overdue: boolean;
  isPriority: boolean;
  checklist: ChecklistItem[];
  recurrence: string;
  // Wall-clock timer (SPEC 4.3): accumulated spent ms; startedAt is the
  // active start in epoch ms, non-null exactly while status is "progress".
  spentMs: number;
  startedAt: number | null;
}

// Distinguishes an explicit null (clear) from an absent field (keep).
// dueTime is NOT NULL backend-side: null/absent keeps the current value.
export interface TaskPatch {
  title?: string;
  comment?: string | null;
  dueDate?: string | null;
  dueTime?: string;
  remindAt?: string | null;
  isPriority?: boolean;
  checklist?: ChecklistItem[];
}

// todos::TodoDto — one plan item of a scope (project chat or harness session).
export interface TodoDto {
  id: number;
  text: string;
  done: boolean;
  position: number;
}

// Payload of the "todos-changed" event emitted by the todowrite tool.
export interface TodosChanged {
  scopeKind: "project" | "session";
  scopeId: number;
}

export interface TasksChanged {
  projectId: number;
}
