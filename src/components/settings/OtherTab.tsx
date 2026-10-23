import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import * as ipc from "../../api/ipc";
import type { BackupStatus, ProjectDto, TrashDto } from "../../api/types";
import { toMessage as errMsg } from "../../shared/errors";
import { confirmRemove } from "../../shared/confirm";
import { type DebouncedConfig } from "./useDebouncedConfig";
import { fmtMb, type AppForm } from "./forms";
import { applyUiScale, getUiScaleError, subscribeUiScaleError } from "../../shared/uiScale";
import { LocationsCard } from "./other/LocationsCard";
import { UiScaleCard } from "./other/UiScaleCard";
import { BackupCard } from "./other/BackupCard";
import { TrashCard } from "./other/TrashCard";
import { ImportExportCard } from "./other/ImportExportCard";
import { MaintenanceCard } from "./other/MaintenanceCard";
import { useImportExport } from "./other/useImportExport";

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
  const scaleError = useSyncExternalStore(
    subscribeUiScaleError,
    getUiScaleError,
    getUiScaleError,
  );

  const [projects, setProjects] = useState<ProjectDto[]>([]);
  const ie = useImportExport(projects);

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
      <LocationsCard workingDir={workingDir} configDir={configDir} />
      <UiScaleCard
        percent={app.form?.uiScalePercent ?? 100}
        scaleError={scaleError}
        onScale={setScale}
      />
      <BackupCard
        app={app}
        backupStatus={backupStatus}
        backups={backups}
        backupBusy={backupBusy}
        backupNote={backupNote}
        onBackup={runBackup}
      />
      <TrashCard
        trash={trash}
        trashError={trashError}
        trashBusyId={trashBusyId}
        onRestore={restoreTrash}
        onPurge={(id, name) => void purgeTrash(id, name)}
      />
      <ImportExportCard projects={projects} ie={ie} />
      <MaintenanceCard
        app={app}
        autostart={autostart}
        autostartBusy={autostartBusy}
        tmpBusy={tmpBusy}
        tmpNote={tmpNote}
        onToggleAutostart={toggleAutostart}
        onClearTmp={clearTmp}
      />
      {error && <div className="set-error">{error}</div>}
    </div>
  );
}
