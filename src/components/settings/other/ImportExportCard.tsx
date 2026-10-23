import type { ProjectDto } from "../../../api/types";
import { Card, Field } from "../shared";
import { type ImportExport } from "./useImportExport";

export function ImportExportCard({
  projects,
  ie,
}: {
  projects: ProjectDto[];
  ie: ImportExport;
}) {
  return (
    <Card
      title="Импорт и экспорт"
      desc="Импорт папок, архивов и Obsidian vault; экспорт проекта или заметок в .zip."
    >
      <Field label="Куда импортировать">
        <select
          value={ie.importTarget}
          onChange={(e) => ie.setImportTarget(e.target.value as "project" | "notes")}
        >
          <option value="project">Проект</option>
          <option value="notes">Заметки и файлы</option>
        </select>
      </Field>
      {ie.importTarget === "project" && (
        <Field label="Проект для импорта">
          <select
            value={ie.importProjectId ?? ""}
            onChange={(e) =>
              ie.setImportProjectId(e.target.value ? Number(e.target.value) : null)
            }
          >
            <option value="">— выбери проект —</option>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </Field>
      )}
      <div className="set-actions">
        <button
          className="ghost-btn"
          disabled={ie.importBusy}
          onClick={() => void ie.runImport("folder")}
        >
          Папка
        </button>
        <button
          className="ghost-btn"
          disabled={ie.importBusy}
          onClick={() => void ie.runImport("archive")}
        >
          Архив .zip
        </button>
        <button
          className="ghost-btn"
          disabled={ie.importBusy}
          onClick={() => void ie.runImport("obsidian")}
        >
          Obsidian vault
        </button>
      </div>
      <div className="set-note">
        Папка и архив идут в выбранную цель, Obsidian vault — в заметки без служебных
        папок (.obsidian, .trash, .git). При совпадении имени добавляется суффикс « copy»,
        файлы не перезаписываются.
      </div>
      {ie.importNote && <div className="set-note">{ie.importNote}</div>}
      {ie.importError && <div className="set-error">{ie.importError}</div>}
      <Field label="Проект для экспорта">
        <select
          value={ie.exportProjectId ?? ""}
          onChange={(e) =>
            ie.setExportProjectId(e.target.value ? Number(e.target.value) : null)
          }
        >
          <option value="">— выбери проект —</option>
          {projects.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      </Field>
      <div className="set-actions">
        <button
          className="ghost-btn"
          disabled={ie.exportBusy}
          onClick={() => void ie.doExport("project")}
        >
          Экспорт проекта
        </button>
        <button
          className="ghost-btn"
          disabled={ie.exportBusy}
          onClick={() => void ie.doExport("notes")}
        >
          Экспорт заметок
        </button>
      </div>
      <div className="set-note">
        Проект и заметки упаковываются в .zip: диалог предлагает имя, папку выбираешь сам.
      </div>
      {ie.exportNote && <div className="set-note">{ie.exportNote}</div>}
      {ie.exportError && <div className="set-error">{ie.exportError}</div>}
    </Card>
  );
}
