import type { TaskDto } from "../../api/types";

// Fractional drop position shared by the kanban board and the task list:
// midpoint between neighbors, first-1 for the top, last+1 for the end.
export function positionForDrop(others: TaskDto[], index: number): number {
  if (others.length === 0) return 1;
  if (index === 0) return others[0].position - 1;
  if (index >= others.length) return others[others.length - 1].position + 1;
  return (others[index - 1].position + others[index].position) / 2;
}

export function blankTask(projectId: number, status: string): TaskDto {
  return {
    id: -1,
    projectId,
    title: "",
    status,
    comment: null,
    dueDate: null,
    dueTime: "07:00",
    remindAt: null,
    position: 0,
    createdAt: "",
    closedAt: null,
    overdue: false,
    isPriority: false,
    checklist: [],
    recurrence: "none",
    spentMs: 0,
    startedAt: null,
  };
}
