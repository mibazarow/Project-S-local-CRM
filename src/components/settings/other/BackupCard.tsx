import type { BackupStatus } from "../../../api/types";
import { Card, Field } from "../shared";
import { fmtRfc, type AppForm } from "../forms";
import { type DebouncedConfig } from "../useDebouncedConfig";

export function BackupCard({
  app,
  backupStatus,
  backups,
  backupBusy,
  backupNote,
  onBackup,
}: {
  app: DebouncedConfig<AppForm>;
  backupStatus: BackupStatus | null;
  backups: string;
  backupBusy: boolean;
  backupNote: string | null;
  onBackup: () => void;
}) {
  return (
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
        <button className="ghost-btn" disabled={backupBusy} onClick={onBackup}>
          {backupBusy ? "Создание…" : "Резервная копия"}
        </button>
      </div>
      {backupNote && <div className="set-note">{backupNote}</div>}
      {backups && <div className="set-note">Резервные копии по умолчанию: {backups}</div>}
    </Card>
  );
}
