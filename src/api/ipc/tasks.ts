// Tasks IPC: CRUD, entity links and per-conversation todo plans.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChecklistItem, TaskDto, TaskPatch, TasksChanged, TodoDto, TodosChanged } from "../types";

export const tasksList = (projectId: number) => invoke<TaskDto[]>("tasks_list", { projectId });

export const taskCreate = (
  projectId: number,
  title: string,
  status?: string,
  dueDate?: string | null,
  dueTime?: string | null,
  isPriority?: boolean,
  checklist?: ChecklistItem[],
) =>
  invoke<TaskDto>("task_create", {
    projectId,
    title,
    status: status ?? null,
    dueDate: dueDate ?? null,
    dueTime: dueTime ?? null,
    isPriority: isPriority ?? null,
    checklist: checklist ?? null,
  });

export const taskUpdate = (id: number, patch: TaskPatch) =>
  invoke<TaskDto>("task_update", { id, patch });

export const taskMove = (id: number, status: string, position: number) =>
  invoke<TaskDto>("task_move", { id, status, position });

export const taskDelete = (id: number) => invoke<void>("task_delete", { id });

// Entity links (links slice): context actions record note ↔ file/chat and
// task ↔ chat after creating the entity; the agent loop records tool-call
// links itself. Deduped backend-side, so a repeated call is a no-op.
export const linkCreate = (
  fromKind: string,
  fromId: string,
  toKind: string,
  toId: string,
) => invoke<void>("link_create", { fromKind, fromId, toKind, toId });

// Per-conversation plans (todowrite/todoread tools): read-only panel data.
export const todosList = (scopeKind: "project" | "session", scopeId: number) =>
  invoke<TodoDto[]>("todos_list", { scopeKind, scopeId });

export const onTasksChanged = (handler: (payload: TasksChanged) => void) =>
  listen<TasksChanged>("tasks-changed", (e) => handler(e.payload));

// Emitted by the todowrite tool after the plan of a scope changed.
export const onTodosChanged = (handler: (payload: TodosChanged) => void) =>
  listen<TodosChanged>("todos-changed", (e) => handler(e.payload));
