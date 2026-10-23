// projects::ProjectDto.

export interface ProjectDto {
  id: number;
  name: string;
  path: string;
  createdAt: string;
  openedAt: string | null;
  position: number;
  fileCount: number;
  taskCount: number;
  overdueCount: number;
  // True when the project has at least one task in "В работе" (status
  // progress); drives the Play indicator on the project row (SPEC 4.1).
  hasActiveTasks: boolean;
  // Per-project kanban column labels, always exactly 3 (projects.rs
  // task_columns); renaming one column replaces the whole triple.
  taskColumns: string[];
  // Per-project file view mode (SPEC 4.2): remembered list/grid, stored in
  // the project DB; unknown backend values normalize to "list".
  filesView: "list" | "grid";
  archived: boolean;
  archivedAt: string | null;
  // Permanent personal task inbox (Q.md item 2): always first, never archived
  // or deleted, files tab hidden.
  builtin: boolean;
}
