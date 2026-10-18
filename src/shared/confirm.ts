// Shared delete confirmation. `what` is the object phrase built at each call
// site (e.g. «файл «x»», «папку «x» со всем содержимым», «заметку «x»»).
export function confirmRemove(what: string): boolean {
  return window.confirm(`Удалить ${what}?`);
}
