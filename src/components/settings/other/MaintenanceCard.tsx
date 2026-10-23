import { Card, Field } from "../shared";
import { type AppForm } from "../forms";
import { type DebouncedConfig } from "../useDebouncedConfig";

export function MaintenanceCard({
  app,
  autostart,
  autostartBusy,
  tmpBusy,
  tmpNote,
  onToggleAutostart,
  onClearTmp,
}: {
  app: DebouncedConfig<AppForm>;
  autostart: boolean | null;
  autostartBusy: boolean;
  tmpBusy: boolean;
  tmpNote: string | null;
  onToggleAutostart: (enabled: boolean) => void;
  onClearTmp: () => void;
}) {
  return (
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
            onChange={(e) => onToggleAutostart(e.target.checked)}
          />
          <span className="track"></span>
        </label>
      </Field>
      <div className="set-actions">
        <button className="ghost-btn" disabled={tmpBusy} onClick={onClearTmp}>
          {tmpBusy ? "Очистка…" : "Очистить Harness/tmp"}
        </button>
      </div>
      {tmpNote && <div className="set-note">{tmpNote}</div>}
    </Card>
  );
}
