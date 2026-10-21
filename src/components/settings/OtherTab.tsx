import { useCallback, useEffect, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import * as ipc from "../../api/ipc";
import type { BackupStatus, ProjectDto, TrashDto } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { Card, Field } from "./shared";
import Segmented from "../shared/Segmented";
import { type DebouncedConfig } from "./useDebouncedConfig";
import { exportStamp, fmtLocal, fmtMb, fmtRfc, type AppForm } from "./forms";
import { UI_SCALE_STEPS, applyUiScale, uiScaleLabel } from "../../shared/uiScale";

export default function OtherTab({
  active,
  error,
  setError,
  app,
}: {
  active: boolean;
  error: string | null;
  setError: (message: string) => void;
  app: DebouncedConfig<AppForm>;
}) {
  const [workingDir, setWorkingDir] = useState<string | null>(null);
  const [configDir, setConfigDir] = useState("");
  const [backups, setBackups] = useState("");
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [tmpBusy, setTmpBusy] = useState(false);
  const [tmpNote, setTmpNote] = useState<string | null>(null);
  const [backupBusy, setBackupBusy] = useState(false);
  const [backupNote, setBackupNote] = useState<string | null>(null);
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [trash, setTrash] = useState<TrashDto[] | null>(null);
  const [trashError, setTrashError] = useState<string | null>(null);
  const [trashBusyId, setTrashBusyId] = useState<number | null>(null);

  // Import/export: folder/.zip/Obsidian imports into a project or
  // the Notes vault, and project/vault → .zip exports.
  const [projects, setProjects] = useState<ProjectDto[]>([]);
  const [importTarget, setImportTarget] = useState<"project" | "notes">("project");
  const [importProjectId, setImportProjectId] = useState<number | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [importNote, setImportNote] = useState<string | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [exportProjectId, setExportProjectId] = useState<number | null>(null);
  const [exportBusy, setExportBusy] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);

  const loadBackupStatus = useCallback(() => {
    void ipc
      .backupStatus()
      .then(setBackupStatus)
      .catch(() => setBackupStatus(null));
  }, []);

  const loadTrash = useCallback(() => {
    void ipc
      .trashList()
      .then((rows) => {
        setTrash(rows);
        setTrashError(null);
      })
      .catch((e) => setTrashError(errMsg(e)));
  }, []);

  const loadProjects = useCallback(() => {
    void ipc
      .listProjects()
      .then(setProjects)
      .catch(() => setProjects([]));
  }, []);

  useEffect(() => {
    void ipc.getWorkingDir().then(setWorkingDir).catch(() => setWorkingDir(null));
    void ipc.configDirPath().then(setConfigDir).catch(() => setConfigDir(""));
    void ipc.backupsDir().then(setBackups).catch(() => setBackups(""));
    void ipc.getAutostart().then(setAutostart).catch(() => setAutostart(null));
    loadBackupStatus();
    loadTrash();
    loadProjects();

    let cancelled = false;
    const unlisteners: (() => void)[] = [];
    const track = (p: Promise<() => void>) =>
      p.then((u) => (cancelled ? u() : unlisteners.push(u)));
    track(
      ipc.onConfigChanged(({ which }) => {
        if (which === "app") loadBackupStatus();
      }),
    );
    track(ipc.onTrashChanged(() => loadTrash()));
    track(ipc.onProjectsChanged(() => loadProjects()));
    return () => {
      cancelled = true;
      unlisteners.forEach((u) => u());
    };
  }, [loadBackupStatus, loadTrash, loadProjects]);

  const clearTmp = () => {
    setTmpBusy(true);
    setTmpNote(null);
    void ipc
      .clearTmp()
      .then((s) =>
        setTmpNote(
          `Удалено файлов: ${s.removedFiles} · освобождено ${fmtMb(s.freedBytes)} МБ`,
        ),
      )
      .catch((e) => setError(errMsg(e)))
      .finally(() => setTmpBusy(false));
  };

  const runBackup = () => {
    setBackupBusy(true);
    setBackupNote(null);
    void ipc
      .createBackup()
      .then((b) => {
        setBackupNote(
          `Готово: ${b.path} · файлов ${b.entries} · ${fmtMb(b.bytes)} МБ`,
        );
        loadBackupStatus();
      })
      .catch((e) => setError(errMsg(e)))
      .finally(() => setBackupBusy(false));
  };

  // Import a folder / .zip / Obsidian vault. folder and archive go into the
  // selected target (project needs a chosen project); Obsidian always lands in
  // the Notes vault. The picker is a directory chooser except for the archive.
  const runImport = async (kind: "folder" | "archive" | "obsidian") => {
    setImportBusy(true);
    setImportNote(null);
    setImportError(null);
    try {
      const picked = await open(
        kind === "archive"
          ? {
              multiple: false,
              title: "Импорт архива .zip",
              filters: [{ name: "Zip-архив", extensions: ["zip"] }],
            }
          : {
              directory: true,
              title: kind === "obsidian" ? "Импорт Obsidian vault" : "Импорт папки",
            },
      );
      if (typeof picked !== "string" || !picked) return;
      if (kind !== "obsidian" && importTarget === "project" && importProjectId == null) {
        setImportError("Выбери проект для импорта");
        return;
      }
      const s =
        kind === "obsidian"
          ? await ipc.importObsidian(picked)
          : kind === "folder"
            ? await ipc.importFolder(importTarget, importProjectId, picked)
            : await ipc.importArchive(importTarget, importProjectId, picked);
      const skipped = s.skipped > 0 ? ` · пропущено папок: ${s.skipped}` : "";
      setImportNote(`Импортировано файлов: ${s.importedFiles}${skipped} → ${s.dest}`);
    } catch (e) {
      setImportError(errMsg(e));
    } finally {
      setImportBusy(false);
    }
  };

  // Export a project or the whole Notes vault to a .zip. The save dialog
  // suggests "<name>-<timestamp>.zip"; the user picks the destination folder.
  const doExport = async (kind: "project" | "notes") => {
    setExportBusy(true);
    setExportNote(null);
    setExportError(null);
    try {
      if (kind === "project" && exportProjectId == null) {
        setExportError("Выбери проект для экспорта");
        return;
      }
      const proj = kind === "project" ? projects.find((p) => p.id === exportProjectId) : null;
      const suggested = `${kind === "project" ? (proj?.name ?? "project") : "notes"}-${exportStamp()}.zip`;
      const dest = await save({
        title: kind === "project" ? "Экспорт проекта в .zip" : "Экспорт заметок в .zip",
        defaultPath: suggested,
        filters: [{ name: "Zip-архив", extensions: ["zip"] }],
      });
      if (typeof dest !== "string" || !dest) return;
      const r =
        kind === "project"
          ? await ipc.exportProject(exportProjectId as number, dest)
          : await ipc.exportNotes(dest);
      setExportNote(`Готово: ${r.path} · файлов ${r.files}`);
    } catch (e) {
      setExportError(errMsg(e));
    } finally {
      setExportBusy(false);
    }
  };

  // Trash restore/purge emit trash-changed, which reloads the list; the busy id
  // only guards the button while the call is in flight.
  const restoreTrash = (id: number) => {
    setTrashBusyId(id);
    setTrashError(null);
    void ipc
      .trashRestore(id)
      .catch((e) => setTrashError(errMsg(e)))
      .finally(() => setTrashBusyId(null));
  };

  const purgeTrash = async (id: number, name: string) => {
    const ok = await confirmRemove({
      title: `Удалить «${name}» навсегда?`,
      message: "Это действие необратимо.",
      confirmLabel: "Удалить навсегда",
    });
    if (!ok) return;
    setTrashBusyId(id);
    setTrashError(null);
    void ipc
      .trashPurge(id)
      .catch((e) => setTrashError(errMsg(e)))
      .finally(() => setTrashBusyId(null));
  };

  const toggleAutostart = (enabled: boolean) => {
    setAutostartBusy(true);
    void ipc
      .setAutostart(enabled)
      .then(setAutostart)
      .catch((e) => setError(errMsg(e)))
      .finally(() => setAutostartBusy(false));
  };

  // UI scale (Q.md item 4): persist through the shared debounced app writer and
  // apply the webview zoom immediately for a live preview.
  const setScale = (percent: number) => {
    app.updateForm({ uiScalePercent: percent });
    void applyUiScale(percent);
  };

  if (!active) return null;
  return (
    <div className="settings-content">
      <Card title="Расположение" desc="Папки, где приложение хранит данные и настройки.">
        <Field label="Рабочая директория" wide>
          <div className="path-value">{workingDir ?? "—"}</div>
        </Field>
        <Field label="Директория настроек" wide>
          <div className="path-value">{configDir || "—"}</div>
        </Field>
      </Card>
      <Card
        title="Интерфейс"
        desc="Масштаб всего интерфейса: сайдбар, чат, задачи, файлы и окна. Применяется сразу, без перезапуска."
      >
        <Field label="Масштаб" hint="Дискретные шаги от 100 % до 200 %.">
          <Segmented
            value={String(app.form?.uiScalePercent ?? 100)}
            options={UI_SCALE_STEPS.map((step) => ({
              value: String(step),
              label: `${step}`,
            }))}
            onChange={(value) => setScale(Number(value))}
            title="Масштаб интерфейса"
          />
          <span className="scale-value">{uiScaleLabel(app.form?.uiScalePercent ?? 100)}</span>
          <button
            type="button"
            className="ghost-btn small"
            onClick={() => setScale(100)}
            disabled={(app.form?.uiScalePercent ?? 100) === 100}
          >
            Сбросить
          </button>
        </Field>
      </Card>
      <Card
        title="Резервные копии"
        desc={
          backupStatus === null
            ? "Статус резервных копий недоступен."
            : `Последняя: ${
                backupStatus.lastBackupAt ? fmtRfc(backupStatus.lastBackupAt) : "ещё не делалась"
              } · на диске: ${backupStatus.count} · ${
                backupStatus.intervalDays === 0
                  ? "автовыключено"
                  : backupStatus.due
                    ? "пора сделать сейчас"
                    : `авто: каждые ${backupStatus.intervalDays} дн.`
              }`
        }
      >
        <Field
          label="Интервал автокопий, дней"
          hint="Каждые N дней при запуске создаётся копия. 0 — выключено."
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={app.form?.backupIntervalDays ?? ""}
            onChange={(e) => app.updateForm({ backupIntervalDays: e.target.value })}
            onFocus={app.focusField("backupIntervalDays")}
            onBlur={app.blurField}
          />
        </Field>
        <Field label="Хранить копий" hint="Сколько последних копий остаётся на диске. 0 — одна.">
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={app.form?.backupKeep ?? ""}
            onChange={(e) => app.updateForm({ backupKeep: e.target.value })}
            onFocus={app.focusField("backupKeep")}
            onBlur={app.blurField}
          />
        </Field>
        <div className="set-actions">
          <button className="ghost-btn" disabled={backupBusy} onClick={runBackup}>
            {backupBusy ? "Создание…" : "Резервная копия"}
          </button>
        </div>
        {backupNote && <div className="set-note">{backupNote}</div>}
        {backups && <div className="set-note">Резервные копии по умолчанию: {backups}</div>}
      </Card>
      <Card
        title="Корзина"
        desc="Удалённые файлы, папки и задачи попадают в корзину и хранятся 30 дней."
      >
        {trash === null ? (
          <div className="set-empty">Загрузка…</div>
        ) : trash.length === 0 ? (
          <div className="set-empty">Корзина пуста.</div>
        ) : (
          <div className="set-list">
            {trash.map((t) => (
              <div key={t.id} className="skill-item">
                <div className="info">
                  <div className="t">{t.name || "(без имени)"}</div>
                  <div className="c">{t.originalPath}</div>
                  <div className="c">удалено {fmtLocal(t.deletedAt)}</div>
                </div>
                <span className="badge">{t.kind}</span>
                <span className="badge st-disabled">{t.scope}</span>
                <span className="item-actions">
                  <button
                    className="ghost-btn small"
                    disabled={trashBusyId !== null}
                    onClick={() => restoreTrash(t.id)}
                  >
                    {trashBusyId === t.id ? "…" : "Восстановить"}
                  </button>
                  <button
                    className="ghost-btn small danger"
                    disabled={trashBusyId !== null}
                    onClick={() => purgeTrash(t.id, t.name)}
                  >
                    Удалить навсегда
                  </button>
                </span>
              </div>
            ))}
          </div>
        )}
        {trashError && <div className="set-error">{trashError}</div>}
      </Card>
      <Card
        title="Импорт и экспорт"
        desc="Импорт папок, архивов и Obsidian vault; экспорт проекта или заметок в .zip."
      >
        <Field label="Куда импортировать">
          <select
            value={importTarget}
            onChange={(e) => setImportTarget(e.target.value as "project" | "notes")}
          >
            <option value="project">Проект</option>
            <option value="notes">Заметки и файлы</option>
          </select>
        </Field>
        {importTarget === "project" && (
          <Field label="Проект для импорта">
            <select
              value={importProjectId ?? ""}
              onChange={(e) =>
                setImportProjectId(e.target.value ? Number(e.target.value) : null)
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
            disabled={importBusy}
            onClick={() => void runImport("folder")}
          >
            Папка
          </button>
          <button
            className="ghost-btn"
            disabled={importBusy}
            onClick={() => void runImport("archive")}
          >
            Архив .zip
          </button>
          <button
            className="ghost-btn"
            disabled={importBusy}
            onClick={() => void runImport("obsidian")}
          >
            Obsidian vault
          </button>
        </div>
        <div className="set-note">
          Папка и архив идут в выбранную цель, Obsidian vault — в заметки без служебных
          папок (.obsidian, .trash, .git). При совпадении имени добавляется суффикс « copy»,
          файлы не перезаписываются.
        </div>
        {importNote && <div className="set-note">{importNote}</div>}
        {importError && <div className="set-error">{importError}</div>}
        <Field label="Проект для экспорта">
          <select
            value={exportProjectId ?? ""}
            onChange={(e) =>
              setExportProjectId(e.target.value ? Number(e.target.value) : null)
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
            disabled={exportBusy}
            onClick={() => void doExport("project")}
          >
            Экспорт проекта
          </button>
          <button
            className="ghost-btn"
            disabled={exportBusy}
            onClick={() => void doExport("notes")}
          >
            Экспорт заметок
          </button>
        </div>
        <div className="set-note">
          Проект и заметки упаковываются в .zip: диалог предлагает имя, папку выбираешь сам.
        </div>
        {exportNote && <div className="set-note">{exportNote}</div>}
        {exportError && <div className="set-error">{exportError}</div>}
      </Card>
      <Card title="Обслуживание" desc="Автоочистка при запуске и запуск с системой.">
        <Field
          label="Автоудаление сессий харнеса, дней"
          hint="Сессии без активности дольше этого срока удаляются при запуске. 0 — отключено."
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={app.form?.sessionTtlDays ?? ""}
            onChange={(e) => app.updateForm({ sessionTtlDays: e.target.value })}
            onFocus={app.focusField("sessionTtlDays")}
            onBlur={app.blurField}
          />
        </Field>
        <Field
          label="Автоочистка Harness/tmp, дней"
          hint="Файлы в Harness/tmp старше этого срока стираются при запуске. 0 — не стирать."
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={app.form?.tmpRetentionDays ?? ""}
            onChange={(e) => app.updateForm({ tmpRetentionDays: e.target.value })}
            onFocus={app.focusField("tmpRetentionDays")}
            onBlur={app.blurField}
          />
        </Field>
        <Field
          label="Хранение журнала запусков, дней"
          hint="Записи журнала старше этого срока удаляются при запуске. 0 — не удалять."
        >
          <input
            className="narrow"
            type="text"
            inputMode="numeric"
            value={app.form?.logRetentionDays ?? ""}
            onChange={(e) => app.updateForm({ logRetentionDays: e.target.value })}
            onFocus={app.focusField("logRetentionDays")}
            onBlur={app.blurField}
          />
        </Field>
        <Field
          label="Запускаться с системой"
          hint="macOS LaunchAgent, чтобы расписания работали без открытого окна. По умолчанию выключено."
        >
          <label className="switch">
            <input
              type="checkbox"
              checked={autostart ?? false}
              disabled={autostart === null || autostartBusy}
              onChange={(e) => toggleAutostart(e.target.checked)}
            />
            <span className="track"></span>
          </label>
        </Field>
        <div className="set-actions">
          <button className="ghost-btn" disabled={tmpBusy} onClick={clearTmp}>
            {tmpBusy ? "Очистка…" : "Очистить Harness/tmp"}
          </button>
        </div>
        {tmpNote && <div className="set-note">{tmpNote}</div>}
      </Card>
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
